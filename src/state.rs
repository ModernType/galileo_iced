//! Shared state of a map widget.
//!
//! [`MapState`] owns the [`galileo::Map`], the galileo event processor and the
//! GPU resources used to draw the map. It is a cheap handle (an `Arc` under the
//! hood): keep one in your application state, and hand clones to
//! [`MapWidget`](crate::MapWidget) from `view`.

use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use galileo::control::{EventProcessor, MapController, UserEventHandler};
use galileo::galileo_types::cartesian::{CartesianPoint2d, Point2, Size as MapSize};
use galileo::galileo_types::geo::impls::GeoPoint2d;
use galileo::galileo_types::geo::{GeoPoint, NewGeoPoint, Projection};
use galileo::layer::attribution::Attribution;
use galileo::render::WgpuRenderer;
use galileo::{Map, MapView, Messenger};
use iced_core::Point;
use iced_futures::futures::channel::mpsc;
use iced_futures::futures::{Stream, StreamExt};
use iced_futures::{BoxStream, Subscription};
use iced_wgpu::wgpu;

/// Minimum render target size. A zero-sized texture is invalid
const MIN_SIZE: u32 = 1;

/// GPU resources belonging to map widget.
pub(crate) struct MapGpu {
    pub renderer: WgpuRenderer,
    pub texture_view: wgpu::TextureView,
    pub bind_group: wgpu::BindGroup,
    pub physical: MapSize<u32>,
}

pub(crate) struct Inner {
    pub map: Map,
    pub gpu: Option<MapGpu>,
    pub logical: (f32, f32),
    pub scale_factor: f32,
    /// `false` until the widget has been laid out at least once with a real
    /// size. Loading layers before that would request tiles for a 1x1 map.
    pub sized: bool,
}

/// Keeps galileo's `tokio::spawn` calls working no matter which thread they
/// are made from.
///
/// `galileo::async_runtime::spawn` is `tokio::spawn` on native targets, and
/// that panics outside of a runtime context. iced's render path gives no
/// such context, so we need to capture runtime ourselves
enum TokioRuntime {
    Ambient(tokio::runtime::Handle),
    Owned(tokio::runtime::Runtime),
}

impl TokioRuntime {
    fn acquire() -> Self {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => Self::Ambient(handle),
            Err(_) => Self::Owned(
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("failed to start a tokio runtime for galileo"),
            ),
        }
    }

    fn handle(&self) -> &tokio::runtime::Handle {
        match self {
            Self::Ambient(handle) => handle,
            Self::Owned(runtime) => runtime.handle(),
        }
    }
}

pub(crate) struct Handle {
    pub inner: Mutex<Inner>,
    pub needs_redraw: Arc<AtomicBool>,
    /// Identifies this map for subscription hashing.
    pub id: u64,
    /// Taken by the first [`MapState::redraw_subscription`] that runs.
    redraw_rx: Mutex<Option<mpsc::UnboundedReceiver<()>>>,
    runtime: TokioRuntime,
    /// Builds the event handler chain for a widget.
    ///
    /// `UserEventHandler` is not `Send`, so the handlers themselves cannot
    /// live in this (shared, `Send + Sync`) state; each widget keeps its own
    /// chain in the iced widget tree and builds it from here.
    handlers: Arc<HandlerFactory>,
}

// Necessary to avoid overflow on auto-trait check
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

type HandlerFactory = dyn Fn() -> Vec<Box<dyn UserEventHandler>> + Send + Sync + 'static;

impl Handle {
    pub fn lock(&self) -> MutexGuard<'_, Inner> {
        // A poisoned lock means a previous frame panicked. The map state is
        // still structurally valid, so recovering beats taking the whole UI
        // down on the next frame.
        self.inner.lock().unwrap_or_else(|err| err.into_inner())
    }

    /// Runs `f` inside a tokio runtime context so galileo can spawn tasks.
    pub fn in_runtime<R>(&self, f: impl FnOnce() -> R) -> R {
        let _guard = self.runtime.handle().enter();
        f()
    }
}

/// Shared state of a Galileo map.
///
/// Cloning is cheap and all clones refer to the same map.
///
/// ```no_run
/// use galileo::MapBuilder;
/// use galileo::layer::raster_tile_layer::RasterTileLayerBuilder;
/// use galileo_iced::MapState;
///
/// let layer = RasterTileLayerBuilder::new_osm().build().unwrap();
/// let state = MapState::new(MapBuilder::default().with_layer(layer).build());
/// ```
#[derive(Clone)]
pub struct MapState {
    pub(crate) handle: Arc<Handle>,
}

impl std::fmt::Debug for MapState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MapState")
            .field("id", &self.handle.id)
            .finish_non_exhaustive()
    }
}

impl MapState {
    /// Creates the state for `map` with the default [`MapController`], which
    /// provides grab-to-pan, wheel zoom and rotation.
    pub fn new(map: Map) -> Self {
        Self::with_handlers(map, Vec::new)
    }

    /// Creates the state for `map` with extra event handlers. The handlers see events before the default [`MapController`]
    pub fn with_handlers<I>(mut map: Map, handlers: impl Fn() -> I + Send + Sync + 'static) -> Self
    where
        I: IntoIterator<Item = Box<dyn UserEventHandler>>,
    {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);

        let (tx, rx) = mpsc::unbounded();
        let needs_redraw = Arc::new(AtomicBool::new(true));
        let messenger = IcedMessenger {
            needs_redraw: needs_redraw.clone(),
            tx,
        };

        map.set_messenger(Some(messenger.clone()));
        for layer in map.layers_mut().iter_mut() {
            layer.set_messenger(Arc::new(messenger.clone()));
        }

        // Placeholder size; replaced on the first layout.
        map.set_size(MapSize::new(MIN_SIZE as f64, MIN_SIZE as f64));

        Self {
            handle: Arc::new(Handle {
                inner: Mutex::new(Inner {
                    map,
                    gpu: None,
                    logical: (0.0, 0.0),
                    scale_factor: 1.0,
                    sized: false,
                }),
                needs_redraw,
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                redraw_rx: Mutex::new(Some(rx)),
                runtime: TokioRuntime::acquire(),
                handlers: Arc::new(move || handlers().into_iter().collect()),
            }),
        }
    }

    pub(crate) fn event_processor(&self) -> EventProcessor {
        let mut events = EventProcessor::default();

        for handler in (self.handle.handlers)() {
            events.add_handler_boxed(handler);
        }

        // Added last so that custom handlers can consume events first
        events.add_handler(MapController::default());

        events
    }

    /// Gives read access to the underlying map
    pub fn with_map<R>(&self, f: impl FnOnce(&Map) -> R) -> R {
        f(&self.handle.lock().map)
    }

    /// Mutates the underlying map. A redraw is requested afterwards
    pub fn with_map_mut<R>(&self, f: impl FnOnce(&mut Map) -> R) -> R {
        let result = {
            let mut inner = self.handle.lock();
            f(&mut inner.map)
        };
        self.request_redraw();
        result
    }

    pub(crate) fn with_map_mut_silent<R>(&self, f: impl FnOnce(&mut Map) -> R) -> R {
        f(&mut self.handle.lock().map)
    }

    pub(crate) fn needs_redraw(&self) -> bool {
        self.handle.needs_redraw.load(Ordering::Relaxed)
    }

    /// Current view of the map (position, resolution, rotation, size).
    pub fn view(&self) -> MapView {
        self.handle.lock().map.view().clone()
    }

    /// Moves the map to `view`
    pub fn set_view(&self, view: MapView) {
        let mut inner = self.handle.lock();
        let current = inner.map.view().clone();
        let view = view
            .with_size(current.size())
            .with_dpi_scale_factor(current.dpi_scale_factor());
        inner.map.set_view(view);
    }

    /// Projects a geographic position to a point inside the map widget, in
    /// logical pixels relative to the widget's top left corner
    ///
    /// Returns `None` when the position is not on screen, which includes the
    /// case of it being behind the camera on a tilted map
    pub fn project(&self, position: &impl GeoPoint<Num = f64>) -> Option<Point> {
        project(&self.view(), position)
    }

    /// The map's current transform, for projecting many positions at once.
    ///
    /// [`project`](Self::project) locks the map and prepares the transform
    /// again for every position; this does both once. Use it whenever the
    /// number of positions is more than a handful — deciding which of a few
    /// thousand markers are on screen, say.
    ///
    /// The result is a snapshot: it stops agreeing with the map as soon as the
    /// view moves, so build one per frame rather than keeping it.
    pub fn projector(&self) -> Option<Projector> {
        Projector::new(&self.view())
    }

    /// The geographic position under a point inside the map widget, given in
    /// logical pixels relative to the widget's top left corner
    ///
    /// Returns `None` for points the map does not reach, which on a tilted map means anything above the
    /// horizon
    pub fn unproject(&self, point: Point) -> Option<GeoPoint2d> {
        unproject(&self.view(), point)
    }

    /// Attributions of all layers currently in the map.
    pub fn attributions(&self) -> Vec<Attribution> {
        self.handle
            .lock()
            .map
            .layers()
            .iter()
            .filter_map(|layer| layer.attribution())
            .collect()
    }

    /// Asks for the map to be re-rendered on the next frame.
    pub fn request_redraw(&self) {
        self.handle.lock().map.redraw();
    }

    /// A subscription that produces a message every time galileo needs the map
    /// to be redrawn. Without it the map stays blank until some other
    /// event happens to wake iced up, because tiles are loaded in the
    /// background
    pub fn redraw_subscription<Message: Send + 'static>(
        &self,
        on_redraw: impl Fn() -> Message + Clone + Send + Sync + 'static,
    ) -> Subscription<Message> {
        // `Subscription::run_with` takes a plain `fn` pointer, so the receiver
        // has to travel inside the (hashable) data argument.
        struct Source(Arc<Handle>);

        impl Hash for Source {
            fn hash<H: Hasher>(&self, state: &mut H) {
                self.0.id.hash(state);
            }
        }

        fn build(source: &Source) -> BoxStream<()> {
            struct Pending;
            impl Stream for Pending {
                type Item = ();

                fn poll_next(
                    self: std::pin::Pin<&mut Self>,
                    _cx: &mut std::task::Context<'_>,
                ) -> std::task::Poll<Option<Self::Item>> {
                    std::task::Poll::Pending
                }
            }

            match source
                .0
                .redraw_rx
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .take()
            {
                Some(rx) => rx.boxed(),
                // The subscription was already running; iced keeps the first
                // instance alive, so this one just has to stay quiet.
                None => Pending.boxed(),
            }
        }

        Subscription::run_with(Source(self.handle.clone()), build).map(move |()| on_redraw())
    }

    /// Returns `true` if map is currently animating a transition
    pub fn is_aniamting(&self) -> bool {
        self.with_map(|map| view_changed(map.view(), map.target_view()))
    }
}

pub(crate) fn view_changed(before: &MapView, after: &MapView) -> bool {
    let moved = match (before.position(), after.position()) {
        (Some(before), Some(after)) => {
            use galileo::galileo_types::geo::GeoPoint;

            before.lat() != after.lat() || before.lon() != after.lon()
        }
        (before, after) => before.is_some() != after.is_some(),
    };

    moved
        || before.resolution() != after.resolution()
        || before.rotation_x() != after.rotation_x()
        || before.rotation_z() != after.rotation_z()
}

/// A view's geographic → screen transform, prepared once and applied many
/// times.
///
/// A projector is a snapshot of one [`MapView`]. Keeping one across frames would
/// project against a view the map has left
pub struct Projector {
    projection: Box<dyn Projection<InPoint = GeoPoint2d, OutPoint = Point2>>,
    /// The rows of `MapView::map_to_scene_transform` that a point on the map
    /// plane needs: x, y and w, each as the coefficients of (map x, map y, 1).
    x: [f64; 3],
    y: [f64; 3],
    w: [f64; 3],
    size: MapSize,
}

impl Projector {
    /// `None` when the view cannot project at all: an empty size, a CRS
    /// without a projection, or no position.
    pub fn new(view: &MapView) -> Option<Self> {
        let transform = view.map_to_scene_transform()?;

        let columns = transform.data.0;
        let row = |index: usize| [columns[0][index], columns[1][index], columns[3][index]];

        Some(Self {
            projection: view.crs().get_projection()?,
            x: row(0),
            y: row(1),
            w: row(3),
            size: view.size(),
        })
    }

    /// Projects `position`, in logical pixels relative to the widget's top
    /// left corner, or `None` when it is not on screen.
    pub fn project(&self, position: &impl GeoPoint<Num = f64>) -> Option<Point> {
        let geo = GeoPoint2d::latlon(position.lat(), position.lon());
        let map = self.projection.project(&geo)?;

        let (x, y) = (map.x(), map.y());
        let apply = |row: [f64; 3]| row[0] * x + row[1] * y + row[2];

        let w = apply(self.w);

        // Galileo divides by `w` without looking at its sign, which turns a
        // position behind the camera of a tilted map into a mirrored point
        // somewhere in front of it. `map_geo_to_screen_clipped` only rejects
        // the ones that land outside the viewport
        if w <= 0.0 {
            return None;
        }

        let screen_x = (apply(self.x) / w + 1.0) * self.size.width() / 2.0;
        let screen_y = (1.0 - apply(self.y) / w) * self.size.height() / 2.0;

        if screen_x < 0.0
            || screen_x > self.size.width()
            || screen_y < 0.0
            || screen_y > self.size.height()
        {
            return None;
        }

        Some(Point::new(screen_x as f32, screen_y as f32))
    }
}

/// Projects `position` through `view`, in logical pixels relative to the
/// widget's top left corner.
///
/// Use [`Projector`] for more than a handful of positions as it caches transformations per map view
pub fn project(view: &MapView, position: &impl GeoPoint<Num = f64>) -> Option<Point> {
    Projector::new(view)?.project(position)
}

/// Finds the geographic position under a widget-local point in `view`.
pub fn unproject(view: &MapView, point: Point) -> Option<GeoPoint2d> {
    view.screen_to_map_geo(Point2::new(point.x as f64, point.y as f64))
}

/// Messenger to transfer galileo requests to iced
#[derive(Clone)]
struct IcedMessenger {
    needs_redraw: Arc<AtomicBool>,
    tx: mpsc::UnboundedSender<()>,
}

impl Messenger for IcedMessenger {
    fn request_redraw(&self) {
        // Only poke the event loop on the 0 -> 1 edge: a single frame will
        // consume every request made since the last render.
        if !self.needs_redraw.swap(true, Ordering::Relaxed) {
            let _ = self.tx.unbounded_send(());
        }
    }
}

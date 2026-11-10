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
use galileo::galileo_types::geo::{GeoPoint, NewGeoPoint};
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

/// Projects `position` through `view`, in logical pixels relative to the
/// widget's top left corner.
pub fn project(view: &MapView, position: &impl GeoPoint<Num = f64>) -> Option<Point> {
    // `map_geo_to_screen` happily returns a mirrored point for positions
    // behind the camera of a tilted map, so take the clipped variant and treat
    // "off screen" and "not projectable" the same way.
    let position = GeoPoint2d::latlon(position.lat(), position.lon());
    let point = view.map_geo_to_screen_clipped(&position)?;

    Some(Point::new(point.x() as f32, point.y() as f32))
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

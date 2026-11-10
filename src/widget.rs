//! The [`MapWidget`] itself

use std::time::{Duration, Instant};

use galileo::MapView;
use galileo::control::{EventProcessor, MouseButton, RawUserEvent, TouchEvent};
use galileo::galileo_types::cartesian::{Point2, Size as MapSize};
use galileo::galileo_types::geo::impls::GeoPoint2d;
use galileo::galileo_types::geo::{GeoPoint, NewGeoPoint};
use iced_core::widget::Operation;
use iced_core::widget::tree::{self, Tree};
use iced_core::{
    Clipboard, Element, Event, Layout, Length, Point, Rectangle, Shell, Size, Vector, Widget,
    alignment, layout, mouse, overlay, renderer, touch,
};

use crate::pipeline::MapPrimitive;
use crate::state::{MapState, project, view_changed};

/// How many galileo "scroll lines" a pixel of trackpad scrolling is worth
///
/// iced reports trackpad scrolling in pixels and mouse wheels in lines,
/// galileo's `MapController` zooms per line
const PIXELS_PER_SCROLL_LINE: f32 = 50.0;

/// Scrolls smaller than this are ignored as noise.
const SCROLL_EPSILON: f64 = 1e-4;

/// How long a button may be held and still count as a click.
const CLICK_DURATION: Duration = Duration::from_millis(500);

/// How far the pointer may travel between press and release and still count
/// as a click, in logical pixels
const CLICK_DISTANCE: f32 = 3.0;

/// Creates a map widget for the given state.
pub fn map<'a, Message, Theme, Renderer>(
    state: &MapState,
) -> MapWidget<'a, Message, Theme, Renderer> {
    MapWidget::new(state)
}

/// A widget that renders a [`galileo::Map`] and lets the user pan, zoom and
/// rotate it
///
/// ```no_run
/// # use galileo_iced::{MapState, MapWidget};
/// # use iced::Element;
/// # #[derive(Debug, Clone)]
/// # enum Message { ViewChanged(galileo::MapView) }
/// fn view(state: &MapState) -> Element<'_, Message> {
///     MapWidget::new(state)
///         .on_view_change(Message::ViewChanged)
///         .into()
/// }
/// ```
///
/// # Pushing widgets to the map
///
/// [`push`](Self::push) places an ordinary iced widget at a geographic
///
/// ```no_run
/// # use galileo_iced::{MapState, MapWidget};
/// # use galileo::galileo_types::latlon;
/// # use iced::Element;
/// # use iced::widget::{button, text};
/// # #[derive(Debug, Clone)]
/// # enum Message { Selected }
/// fn view(state: &MapState) -> Element<'_, Message> {
///     MapWidget::new(state)
///         .push(button(text("Seoul")).on_press(Message::Selected), latlon!(37.566, 126.978))
///         .into()
/// }
/// ```
pub struct MapWidget<'a, Message, Theme, Renderer> {
    state: MapState,
    width: Length,
    height: Length,
    on_view_change: Option<Box<dyn Fn(MapView) -> Message>>,
    on_cursor_move: Option<Box<dyn Fn(Option<GeoPoint2d>) -> Message>>,
    on_click: Option<Box<dyn Fn(Click) -> Message>>,
    grab_cursor: bool,
    // Children
    pins: Vec<Pin>,
    children: Vec<Element<'a, Message, Theme, Renderer>>,
}

/// A click on the map: a button pressed and released without the pointer
/// going anywhere much in between
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Click {
    /// Which button was pressed
    pub button: mouse::Button,
    /// Where on the map the click landed, or `None` for a click above the
    /// horizon of a tilted map
    pub position: Option<GeoPoint2d>,
    /// Where in the widget it landed, in logical pixels from the top left
    /// corner
    pub cursor: Point,
}

struct Press {
    button: mouse::Button,
    at: Instant,
    cursor: Point,
    travelled: f32,
}

struct Pin {
    position: GeoPoint2d,
    horizontal: alignment::Horizontal,
    vertical: alignment::Vertical,
}

impl Pin {
    /// Top left corner of a widget of `size` anchored at `point`.
    fn anchor(&self, point: Point, size: Size) -> Point {
        let x = match self.horizontal {
            alignment::Horizontal::Left => point.x,
            alignment::Horizontal::Center => point.x - size.width / 2.0,
            alignment::Horizontal::Right => point.x - size.width,
        };

        let y = match self.vertical {
            alignment::Vertical::Top => point.y,
            alignment::Vertical::Center => point.y - size.height / 2.0,
            alignment::Vertical::Bottom => point.y - size.height,
        };

        Point::new(x, y)
    }
}

impl<'a, Message, Theme, Renderer> MapWidget<'a, Message, Theme, Renderer> {
    /// Creates a widget that fills the space available to it.
    pub fn new(state: &MapState) -> Self {
        Self {
            state: state.clone(),
            width: Length::Fill,
            height: Length::Fill,
            on_view_change: None,
            on_cursor_move: None,
            on_click: None,
            grab_cursor: true,
            pins: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Puts `element` on a geographic position, centred on it
    pub fn push(
        self,
        element: impl Into<Element<'a, Message, Theme, Renderer>>,
        position: impl GeoPoint<Num = f64>,
    ) -> Self {
        self.push_anchored(
            element,
            position,
            alignment::Horizontal::Center,
            alignment::Vertical::Center,
        )
    }

    /// Pins `element` to a geographic position
    pub fn push_anchored(
        mut self,
        element: impl Into<Element<'a, Message, Theme, Renderer>>,
        position: impl GeoPoint<Num = f64>,
        horizontal: alignment::Horizontal,
        vertical: alignment::Vertical,
    ) -> Self {
        self.pins.push(Pin {
            position: GeoPoint2d::latlon(position.lat(), position.lon()),
            horizontal,
            vertical,
        });
        self.children.push(element.into());

        self
    }

    /// Sets the width of the widget.
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    /// Sets the height of the widget.
    pub fn height(mut self, height: impl Into<Length>) -> Self {
        self.height = height.into();
        self
    }

    /// Produces a message whenever user interaction moves the map. Only fired for interaction, not for [`MapState::set_view`]
    pub fn on_view_change(mut self, on_view_change: impl Fn(MapView) -> Message + 'static) -> Self {
        self.on_view_change = Some(Box::new(on_view_change));
        self
    }

    /// Whether to show grab/grabbing mouse cursors over the map
    pub fn grab_cursor(mut self, grab_cursor: bool) -> Self {
        self.grab_cursor = grab_cursor;
        self
    }

    /// Produces a message with the position the pointer is over, and `None`
    /// once it leaves the map or comes to rest above the horizon of a tilted
    /// one. The position comes out as WGS 84 latitude and longitude
    pub fn on_cursor_move(
        mut self,
        on_cursor_move: impl Fn(Option<GeoPoint2d>) -> Message + 'static,
    ) -> Self {
        self.on_cursor_move = Some(Box::new(on_cursor_move));
        self
    }

    /// Produces a message when a mouse button is clicked on the map, as
    /// opposed to used to drag it.
    ///
    /// ```no_run
    /// # use galileo_iced::{Click, MapState, MapWidget};
    /// # use iced::Element;
    /// # use iced::mouse;
    /// # #[derive(Debug, Clone)]
    /// # enum Message { Inspect(Click), Menu(Click) }
    /// # fn view(state: &MapState) -> Element<'_, Message> {
    /// MapWidget::new(state)
    ///     .on_click(|click| match click.button {
    ///         mouse::Button::Right => Message::Menu(click),
    ///         _ => Message::Inspect(click),
    ///     })
    ///     .into()
    /// # }
    /// ```
    pub fn on_click(mut self, on_click: impl Fn(Click) -> Message + 'static) -> Self {
        self.on_click = Some(Box::new(on_click));
        self
    }

    fn track_click(
        &self,
        controls: &mut Controls,
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
        shell: &mut Shell<'_, Message>,
    ) {
        let Some(on_click) = &self.on_click else {
            return;
        };

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(button)) => {
                controls.press = cursor.position_in(bounds).map(|cursor| Press {
                    button: *button,
                    at: Instant::now(),
                    cursor,
                    travelled: 0.0,
                });
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                if let Some(press) = &mut controls.press {
                    let moved = Point::new(position.x - bounds.x, position.y - bounds.y);

                    press.travelled = press.travelled.max(press.cursor.distance(moved));
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(button)) => {
                let Some(press) = controls.press.take_if(|press| press.button == *button) else {
                    return;
                };

                if press.at.elapsed() <= CLICK_DURATION && press.travelled <= CLICK_DISTANCE {
                    shell.publish(on_click(Click {
                        button: *button,
                        position: self.state.unproject(press.cursor),
                        cursor: press.cursor,
                    }));
                }
            }
            _ => {}
        }
    }

    fn publish_cursor(
        &self,
        controls: &mut Controls,
        event: &Event,
        bounds: Rectangle,
        is_over: bool,
        shell: &mut Shell<'_, Message>,
    ) {
        let Some(on_cursor_move) = &self.on_cursor_move else {
            return;
        };

        let left = match event {
            Event::Mouse(mouse::Event::CursorMoved { position }) if is_over => {
                let local = Point::new(position.x - bounds.x, position.y - bounds.y);

                controls.cursor_inside = true;
                shell.publish(on_cursor_move(self.state.unproject(local)));

                return;
            }
            Event::Mouse(mouse::Event::CursorMoved { .. } | mouse::Event::CursorLeft) => true,
            _ => false,
        };

        if left && controls.cursor_inside {
            controls.cursor_inside = false;
            shell.publish(on_cursor_move(None));
        }
    }

    fn publish_animation_step(&self, controls: &mut Controls, shell: &mut Shell<'_, Message>) {
        let Some(reported) = controls.animating.as_ref() else {
            return;
        };

        let Some(on_view_change) = &self.on_view_change else {
            return;
        };

        let current = self.state.view();

        if view_changed(reported, &current) {
            shell.publish(on_view_change(current.clone()));
            controls.animating = Some(current);
        }

        // The final step lands exactly on the target, so this runs one frame
        // after the last move: the view published above is the settled one.
        if !self.state.is_aniamting() {
            controls.animating = None;
        }
    }
}

struct Controls {
    events: EventProcessor,
    cursor_inside: bool,
    press: Option<Press>,
    animating: Option<MapView>,
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for MapWidget<'_, Message, Theme, Renderer>
where
    Renderer: iced_wgpu::primitive::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Controls>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Controls {
            events: self.state.event_processor(),
            cursor_inside: false,
            press: None,
            animating: None,
        })
    }

    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&self.children);
    }

    fn size(&self) -> Size<Length> {
        Size::new(self.width, self.height)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let size = limits.resolve(self.width, self.height, Size::ZERO);

        if self.children.is_empty() {
            return layout::Node::new(size);
        }

        let view = self
            .state
            .view()
            .with_size(MapSize::new(size.width as f64, size.height as f64));

        let child_limits = layout::Limits::new(Size::ZERO, size);

        let children = self
            .children
            .iter_mut()
            .zip(&self.pins)
            .zip(&mut tree.children)
            .map(|((element, pin), tree)| {
                let node = element
                    .as_widget_mut()
                    .layout(tree, renderer, &child_limits);

                match project(&view, &pin.position) {
                    Some(point) => {
                        let corner = pin.anchor(point, node.size());
                        node.move_to(corner)
                    }
                    // Off screen, or behind the camera on a tilted map
                    None => node.move_to(Point::new(size.width, size.height)),
                }
            })
            .collect();

        layout::Node::with_children(size, children)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            self.children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
                .for_each(|((element, tree), layout)| {
                    element
                        .as_widget_mut()
                        .operate(tree, layout, renderer, operation);
                });
        });
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let Tree {
            state,
            children: child_trees,
            ..
        } = tree;

        let mut cursor = cursor;

        for ((element, tree), layout) in self
            .children
            .iter_mut()
            .rev()
            .zip(child_trees.iter_mut().rev())
            .zip(layout.children().rev())
        {
            element.as_widget_mut().update(
                tree, event, layout, cursor, renderer, clipboard, shell, viewport,
            );

            if shell.is_event_captured() {
                return;
            }

            let interaction = element
                .as_widget()
                .mouse_interaction(tree, layout, cursor, viewport, renderer);

            if interaction != mouse::Interaction::None {
                cursor = cursor.levitate();
            }
        }

        let controls = state.downcast_mut::<Controls>();

        if let Event::Window(iced_core::window::Event::RedrawRequested(_)) = event {
            self.publish_animation_step(controls, shell);

            if self.state.needs_redraw() || self.state.is_aniamting() {
                shell.request_redraw();
            }

            return;
        }

        let is_dragging = controls.events.is_dragging();
        let is_over = cursor.is_over(bounds);

        if let Some(raw_event) = translate(event, bounds, is_over, is_dragging) {
            let before = self.on_view_change.as_ref().map(|_| self.state.view());

            self.state
                .with_map_mut_silent(|map| controls.events.handle(raw_event, map));

            if self.state.needs_redraw() || self.state.is_aniamting() {
                shell.request_redraw();
            }

            if is_over || is_dragging {
                shell.capture_event();
            }

            if let (Some(on_view_change), Some(before)) = (&self.on_view_change, before) {
                let after = self.state.view();

                let reported = if view_changed(&before, &after) {
                    shell.publish(on_view_change(after.clone()));
                    after
                } else {
                    before
                };

                controls.animating = self.state.is_aniamting().then_some(reported);
            }
        }

        self.track_click(controls, event, bounds, cursor, shell);
        self.publish_cursor(controls, event, bounds, is_over, shell);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        let pinned = self
            .children
            .iter()
            .rev()
            .zip(tree.children.iter().rev())
            .zip(layout.children().rev())
            .map(|((element, tree), layout)| {
                element
                    .as_widget()
                    .mouse_interaction(tree, layout, cursor, viewport, renderer)
            })
            .find(|interaction| *interaction != mouse::Interaction::None);

        if let Some(interaction) = pinned {
            return interaction;
        }

        if !self.grab_cursor {
            return mouse::Interaction::None;
        }

        if tree.state.downcast_ref::<Controls>().events.is_dragging() {
            mouse::Interaction::Grabbing
        } else if cursor.is_over(layout.bounds()) {
            mouse::Interaction::Grab
        } else {
            mouse::Interaction::None
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();

        let Some(visible) = bounds.intersection(viewport) else {
            return;
        };

        renderer.draw_primitive(
            bounds,
            MapPrimitive {
                state: self.state.clone(),
            },
        );

        if self.children.is_empty() {
            return;
        }

        renderer.with_layer(visible, |renderer| {
            for ((element, tree), layout) in self
                .children
                .iter()
                .zip(&tree.children)
                .zip(layout.children())
            {
                element
                    .as_widget()
                    .draw(tree, renderer, theme, style, layout, cursor, &visible);
            }
        });
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        overlay::from_children(
            &mut self.children,
            tree,
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message, Theme, Renderer> From<MapWidget<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Renderer: iced_wgpu::primitive::Renderer + 'a,
    Message: 'a,
    Theme: 'a,
{
    fn from(widget: MapWidget<'a, Message, Theme, Renderer>) -> Self {
        Element::new(widget)
    }
}

/// Converts an iced event into galileo's raw event, in widget-local logical
/// pixels.
fn translate(
    event: &Event,
    bounds: Rectangle,
    is_over: bool,
    is_dragging: bool,
) -> Option<RawUserEvent> {
    let local = |position: Point| {
        Point2::new(
            (position.x - bounds.x) as f64,
            (position.y - bounds.y) as f64,
        )
    };

    match event {
        Event::Mouse(mouse::Event::ButtonPressed(button)) if is_over => {
            Some(RawUserEvent::ButtonPressed(mouse_button(*button)))
        }
        Event::Mouse(mouse::Event::ButtonReleased(button)) if is_over || is_dragging => {
            Some(RawUserEvent::ButtonReleased(mouse_button(*button)))
        }
        Event::Mouse(mouse::Event::CursorMoved { position }) if is_over || is_dragging => {
            Some(RawUserEvent::PointerMoved(local(*position)))
        }
        Event::Mouse(mouse::Event::WheelScrolled { delta }) if is_over => {
            let lines = match delta {
                mouse::ScrollDelta::Lines { y, .. } => *y as f64,
                mouse::ScrollDelta::Pixels { y, .. } => (*y / PIXELS_PER_SCROLL_LINE) as f64,
            };

            (lines.abs() >= SCROLL_EPSILON).then_some(RawUserEvent::Scroll(lines))
        }
        Event::Touch(touch_event) => {
            let (id, position) = match touch_event {
                touch::Event::FingerPressed { id, position }
                | touch::Event::FingerMoved { id, position }
                | touch::Event::FingerLifted { id, position }
                | touch::Event::FingerLost { id, position } => (*id, *position),
            };

            if !is_over && !is_dragging {
                return None;
            }

            let event = TouchEvent {
                touch_id: id.0,
                position: local(position),
            };

            Some(match touch_event {
                touch::Event::FingerPressed { .. } => RawUserEvent::TouchStart(event),
                touch::Event::FingerMoved { .. } => RawUserEvent::TouchMove(event),
                touch::Event::FingerLifted { .. } | touch::Event::FingerLost { .. } => {
                    RawUserEvent::TouchEnd(event)
                }
            })
        }
        _ => None,
    }
}

fn mouse_button(button: mouse::Button) -> MouseButton {
    match button {
        mouse::Button::Left => MouseButton::Left,
        mouse::Button::Right => MouseButton::Right,
        mouse::Button::Middle => MouseButton::Middle,
        _ => MouseButton::Other,
    }
}

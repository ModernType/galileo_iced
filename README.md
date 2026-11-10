# galileo-iced

A [Galileo](galileo) map widget for the [`iced`] GUI framework.

# Usage

Keep a [`MapState`] in your application state, build a [`MapWidget`] from
it in `view`, and return [`MapState::redraw_subscription`] from
`subscription`:

```
use galileo::MapBuilder;
use galileo::layer::raster_tile_layer::RasterTileLayerBuilder;
use galileo_iced::{MapState, MapWidget};
use iced::{Element, Subscription};

#[derive(Debug, Clone)]
enum Message {
    MapRedraw,
}

struct App {
    map: MapState,
}

impl App {
    fn new() -> Self {
        let layer = RasterTileLayerBuilder::new_osm()
            .build()
            .expect("failed to build the OSM layer");

        Self {
            map: MapState::new(MapBuilder::default().with_layer(layer).build()),
        }
    }

    fn update(&mut self, _message: Message) {}

    fn view(&self) -> Element<'_, Message> {
        MapWidget::new(&self.map).into()
    }

    fn subscription(&self) -> Subscription<Message> {
        self.map.redraw_subscription(|| Message::MapRedraw)
    }
}
```

The subscription is not optional: tiles load in the background, and without
it iced has no reason to repaint when they arrive.

# Overlays

[`MapWidget::push`] puts an ordinary iced widget to a geographic position.
It keeps its place as the map moves, and behaves like any other child in any other container.

```
fn view(state: &MapState) -> Element<'_, Message> {
  MapWidget::new(state)
    .push(button(text("Seoul")).on_press(Message::Selected), latlon!(37.566, 126.978))
    .into()
}
```

[`MapState::project`] exposes the same geographic-to-screen conversion for
cases that need the coordinates directly.

# Clicks

[`MapWidget::on_click`] reports a button pressed and released on the map
without dragging it.

```
fn view(state: &MapState) -> Element<'_, Message> {
  MapWidget::new(state)
    .on_click(|click| match click.button {
        mouse::Button::Right => Message::Menu(click),
        _ => Message::Inspect(click),
    })
    .into()
}
```

# Showing where the pointer is

[`MapWidget::on_cursor_move`] reports the position under the pointer, as a
`GeoPoint2d`, and `None` once it leaves the map:

```
fn view(state: &MapState) -> Element<'_, Message> {
  MapWidget::new(state)
    .on_cursor_move(Message::CursorMoved)
    .into()
}
```

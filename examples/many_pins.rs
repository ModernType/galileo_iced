//! Stress test: thousands of clickable pins on the map.

use std::cell::Cell;
use std::time::{Duration, Instant};

use galileo::galileo_types::geo::GeoPoint;
use galileo::galileo_types::geo::impls::GeoPoint2d;
use galileo::galileo_types::latlon;
use galileo::layer::raster_tile_layer::RasterTileLayerBuilder;
use galileo::{Map, MapBuilder};
use galileo_iced::{MapState, MapWidget};
use iced::widget::{button, column, container, row, stack, text};
use iced::{Element, Font, Length, Subscription};

const COUNTS: [usize; 6] = [0, 100, 1000, 5000, 10_000, 25_000];

/// Box the random positions fall in
const BOX: (f64, f64, f64, f64) = (33.0, 39.5, 124.5, 131.5);

/// How long frame statistics are averaged over before they are reported.
const WINDOW: Duration = Duration::from_millis(500);

fn main() -> iced::Result {
    let settings = Settings {
        count: 1000,
        zoom: 7,
        cull: false,
        stress: false,
        report: true,
    };

    iced::application(move || App::new(settings), App::update, App::view)
        .title("galileo_iced — many pins")
        .subscription(App::subscription)
        .window_size((1024.0, 768.0))
        .run()
}

/// What the command line asked for.
#[derive(Clone, Copy)]
struct Settings {
    count: usize,
    zoom: u32,
    cull: bool,
    stress: bool,
    report: bool,
}

struct App {
    map: MapState,
    points: Vec<GeoPoint2d>,
    count: usize,
    cull: bool,
    stress: bool,
    selected: Option<usize>,
    last_click: Option<String>,
    stats: Stats,
    report: bool,
}

#[derive(Default)]
struct Stats {
    frames: Cell<u32>,
    build: Cell<Duration>,
    opened: Cell<Option<Instant>>,
    reported: Cell<(f32, Duration, usize)>,
}

#[derive(Debug, Clone)]
enum Message {
    MapRedraw,
    PointClicked(usize),
    SetCount(usize),
    ToggleCull,
    ToggleStress,
    Frame,
}

impl App {
    fn new(settings: Settings) -> Self {
        Self {
            map: MapState::new(create_map(settings.zoom)),
            points: random_points(COUNTS[COUNTS.len() - 1].max(settings.count)),
            count: settings.count,
            cull: settings.cull,
            stress: settings.stress,
            selected: None,
            last_click: None,
            stats: Stats::default(),
            report: settings.report,
        }
    }

    fn update(&mut self, message: Message) {
        match message {
            Message::MapRedraw | Message::Frame => {}
            Message::PointClicked(index) => {
                let text = format!("#{index}  {}", latlon_text(&self.points[index]));

                println!("clicked {text}");
                self.selected = Some(index);
                self.last_click = Some(text);
            }
            Message::SetCount(count) => self.count = count,
            Message::ToggleCull => self.cull = !self.cull,
            Message::ToggleStress => self.stress = !self.stress,
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let started = Instant::now();

        let mut map = MapWidget::new(&self.map);
        let mut shown = 0;

        let projector = self.cull.then(|| self.map.projector()).flatten();

        for (index, position) in self.points[..self.count.min(self.points.len())]
            .iter()
            .enumerate()
        {
            if let Some(projector) = &projector
                && projector.project(position).is_none()
            {
                continue;
            }

            map = map.push(pin(index, self.selected == Some(index)), *position);
            shown += 1;
        }

        if self.stats.record(started.elapsed(), shown) && self.report {
            let (fps, build, shown) = self.stats.reported.get();

            println!(
                "{:>6} pins  cull {} | {fps:5.1} fps | {:6.2} ms frame | \
                 {:6.2} ms view | {shown} shown",
                self.count,
                if self.cull { "on " } else { "off" },
                1000.0 / fps,
                build.as_secs_f32() * 1000.0,
            );
        }

        stack![map, self.hud()].into()
    }

    fn subscription(&self) -> Subscription<Message> {
        let redraw = self.map.redraw_subscription(|| Message::MapRedraw);

        if self.stress {
            Subscription::batch([redraw, iced::window::frames().map(|_| Message::Frame)])
        } else {
            redraw
        }
    }

    fn hud(&self) -> Element<'_, Message> {
        let (fps, build, shown) = self.stats.reported.get();

        let counts = row(COUNTS.iter().map(|&count| {
            toggle(
                count.to_string(),
                self.count == count,
                Message::SetCount(count),
            )
        }))
        .spacing(4);

        let switches = row![
            toggle("cull".to_owned(), self.cull, Message::ToggleCull),
            toggle("stress".to_owned(), self.stress, Message::ToggleStress),
        ]
        .spacing(4);

        let report = format!(
            "pins      {} requested, {shown} in the tree\n\
             view      {:.2} ms building the map widget\n\
             frames    {fps:.0}/s{}\n\
             {}",
            self.count,
            build.as_secs_f32() * 1000.0,
            if self.stress {
                ""
            } else {
                "  (idle — turn on stress)"
            },
            self.last_click
                .clone()
                .unwrap_or_else(|| "click a pin".to_owned()),
        );

        container(column![counts, switches, label(report)].spacing(4))
            .padding(8)
            .align_top(Length::Fill)
            .into()
    }
}

impl Stats {
    fn record(&self, build: Duration, shown: usize) -> bool {
        let now = Instant::now();
        let opened = self.opened.get().unwrap_or(now);

        self.frames.set(self.frames.get() + 1);
        self.build.set(self.build.get() + build);

        let elapsed = now.duration_since(opened);

        if self.opened.get().is_none() {
            self.opened.set(Some(now));

            return false;
        }

        if elapsed < WINDOW {
            return false;
        }

        let frames = self.frames.get();

        self.reported.set((
            frames as f32 / elapsed.as_secs_f32(),
            self.build.get() / frames,
            shown,
        ));

        self.opened.set(Some(now));
        self.frames.set(0);
        self.build.set(Duration::ZERO);

        true
    }
}

fn pin(index: usize, selected: bool) -> Element<'static, Message> {
    let style = if selected {
        button::primary
    } else {
        button::danger
    };

    button("")
        .width(10)
        .height(10)
        .padding(0)
        .style(style)
        .on_press(Message::PointClicked(index))
        .into()
}

fn toggle(name: String, active: bool, message: Message) -> Element<'static, Message> {
    let style = if active {
        button::primary
    } else {
        button::secondary
    };

    button(text(name).size(12))
        .padding([2, 6])
        .style(style)
        .on_press(message)
        .into()
}

fn label(content: String) -> Element<'static, Message> {
    container(text(content).size(12).font(Font::MONOSPACE))
        .padding([4, 8])
        .style(container::dark)
        .into()
}

/// A position written out for the HUD.
fn latlon_text(position: &GeoPoint2d) -> String {
    format!("{:.5}, {:.5}", position.lat(), position.lon())
}

/// Positions spread over [`BOX`], from a fixed seed so two runs are comparable.
fn random_points(count: usize) -> Vec<GeoPoint2d> {
    let (min_lat, max_lat, min_lon, max_lon) = BOX;
    let mut state = 0x2545_f491_4f6c_dd1d_u64;

    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;

        (state.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
    };

    (0..count)
        .map(|_| {
            let lat = min_lat + next() * (max_lat - min_lat);
            let lon = min_lon + next() * (max_lon - min_lon);

            latlon!(lat, lon)
        })
        .collect()
}

fn create_map(zoom: u32) -> Map {
    let osm = RasterTileLayerBuilder::new_osm()
        .with_file_cache_checked(".tile_cache")
        .build()
        .expect("failed to create the OSM layer");

    MapBuilder::default()
        .with_latlon(36.3, 128.0)
        .with_z_level(zoom)
        .with_layer(osm)
        .build()
}

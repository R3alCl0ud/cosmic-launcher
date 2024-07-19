use crate::app::iced::event::listen_raw;
use crate::components;
use crate::subscriptions::launcher;
use clap::Parser;
use cosmic::app::{Command, Core, CosmicFlags, DbusActivationDetails, Settings};
use cosmic::cctk::sctk;
use cosmic::iced::alignment::{Horizontal, Vertical};
use cosmic::iced::event::Status;
use cosmic::iced::id::Id;
use cosmic::iced::wayland::actions::layer_surface::SctkLayerSurfaceSettings;
use cosmic::iced::wayland::actions::popup::{SctkPopupSettings, SctkPositioner};
use cosmic::iced::wayland::layer_surface::{
    destroy_layer_surface, get_layer_surface, Anchor, KeyboardInteractivity,
};
use cosmic::iced::widget::{column, container, Column};
use cosmic::iced::{self, Length, Subscription};
use cosmic::iced_core::keyboard::key::Named;
use cosmic::iced_core::widget::operation::focusable::find_focused;
use cosmic::iced_core::{Border, Padding, Point, Rectangle, Shadow};
use cosmic::iced_runtime::core::event::wayland::LayerEvent;
use cosmic::iced_runtime::core::event::{wayland, PlatformSpecific};
use cosmic::iced_runtime::core::layout::Limits;
use cosmic::iced_runtime::core::window::Id as SurfaceId;
use cosmic::iced_sctk::commands;
use cosmic::iced_sctk::commands::activation::request_token;
use cosmic::iced_style::{application, container::Appearance as ContainerAppearance};
use cosmic::iced_widget::row;
use cosmic::theme::{self, Button, Container};
use cosmic::widget::icon::{from_name, IconFallback};
use cosmic::widget::{
    button, divider, horizontal_space, icon, mouse_area, scrollable, text,
    text_input::{self, StyleSheet as TextInputStyleSheet},
};
use cosmic::{keyboard_nav, Element, Theme};
use iced::keyboard::Key;
use iced::widget::vertical_space;
use iced::{Alignment, Color};
use once_cell::sync::Lazy;
use pop_launcher::{ContextOption, GpuPreference, IconSource, SearchResult};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::str::FromStr;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{debug, error, info};
use unicode_truncate::UnicodeTruncateStr;
use unicode_width::UnicodeWidthStr;

static INPUT_ID: Lazy<Id> = Lazy::new(|| Id::new("input_id"));
static RESULT_IDS: Lazy<[Id; 10]> = Lazy::new(|| {
    (0..10)
        .map(|id| Id::new(id.to_string()))
        .collect::<Vec<_>>()
        .try_into()
        .unwrap()
});
pub(crate) static WINDOW_ID: Lazy<SurfaceId> = Lazy::new(SurfaceId::unique);
pub(crate) static MENU_ID: Lazy<SurfaceId> = Lazy::new(SurfaceId::unique);

#[derive(Parser, Debug, Serialize, Deserialize, Clone)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
pub struct Args {
    #[clap(subcommand)]
    pub subcommand: Option<LauncherCommands>,
}

#[derive(Debug, Serialize, Deserialize, Clone, clap::Subcommand)]
pub enum LauncherCommands {
    #[clap(about = "Toggle the launcher and switch to the alt-tab view")]
    AltTab,
    #[clap(about = "Toggle the launcher and switch to the alt-tab view")]
    ShiftAltTab,
}

impl ToString for LauncherCommands {
    fn to_string(&self) -> String {
        serde_json::ser::to_string(self).unwrap()
    }
}

impl FromStr for LauncherCommands {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_json::de::from_str(s)
    }
}

impl CosmicFlags for Args {
    type SubCommand = LauncherCommands;
    type Args = Vec<String>;

    fn action(&self) -> Option<&LauncherCommands> {
        self.subcommand.as_ref()
    }
}

pub fn run() -> cosmic::iced::Result {
    let args = Args::parse();
    cosmic::app::run_single_instance::<CosmicLauncher>(
        Settings::default()
            .antialiasing(true)
            .client_decorations(true)
            .debug(false)
            .default_text_size(16.0)
            .scale_factor(1.0)
            .no_main_window(true)
            .exit_on_close(false),
        args,
    )
}

pub fn menu_button<'a, Message>(
    content: impl Into<Element<'a, Message>>,
) -> cosmic::widget::Button<'a, Message> {
    button(content)
        .style(Button::AppletMenu)
        .padding(menu_control_padding())
        .width(Length::Fill)
}

pub fn menu_control_padding() -> Padding {
    let theme = cosmic::theme::active();
    let cosmic = theme.cosmic();
    [cosmic.space_xxs(), cosmic.space_m()].into()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceState {
    Visible,
    Hidden,
    WaitingToBeShown,
}

#[derive(Clone)]
pub struct CosmicLauncher {
    core: Core,
    input_value: String,
    surface_state: SurfaceState,
    launcher_items: Vec<SearchResult>,
    tx: Option<mpsc::Sender<launcher::Request>>,
    menu: Option<(u32, Vec<ContextOption>)>,
    cursor_position: Option<Point<f32>>,
    focused: usize,
    last_hide: Instant,
    alt_tab: bool,
    queue: VecDeque<Message>,
}

#[derive(Debug, Clone)]
pub enum Message {
    InputChanged(String),
    Backspace,
    TabPress,
    CompleteFocusedId(Id),
    Activate(Option<usize>),
    Context(usize),
    MenuButton(u32, u32),
    CloseContextMenu,
    CursorMoved(Point<f32>),
    Hide,
    LauncherEvent(launcher::Event),
    Layer(LayerEvent),
    KeyboardNav(keyboard_nav::Message),
    ActivationToken(Option<String>, String, GpuPreference),
    AltTab,
    ShiftAltTab,
    AltRelease,
}

impl CosmicLauncher {
    fn request(&self, r: launcher::Request) {
        debug!("request: {:?}", r);
        if let Some(tx) = &self.tx {
            if let Err(e) = tx.blocking_send(r) {
                error!("tx: {e}");
            }
        } else {
            info!("tx not found");
        }
    }

    fn show(&mut self) -> Command<Message> {
        self.surface_state = SurfaceState::Visible;

        get_layer_surface(SctkLayerSurfaceSettings {
            id: *WINDOW_ID,
            keyboard_interactivity: KeyboardInteractivity::Exclusive,
            anchor: Anchor::TOP,
            namespace: "launcher".into(),
            size: None,
            margin: iced::wayland::actions::layer_surface::IcedMargin {
                top: 16,
                ..Default::default()
            },
            size_limits: Limits::NONE.min_width(1.0).min_height(1.0).max_width(600.0),
            ..Default::default()
        })
    }

    fn hide(&mut self) -> Command<Message> {
        self.input_value.clear();
        self.focused = 0;
        self.alt_tab = false;
        self.queue.clear();

        self.request(launcher::Request::Close);

        let mut commands = Vec::new();

        if self.surface_state == SurfaceState::Visible {
            commands.push(destroy_layer_surface(*WINDOW_ID));
            if self.menu.take().is_some() {
                commands.push(commands::popup::destroy_popup(*MENU_ID));
            }
        }

        self.surface_state = SurfaceState::Hidden;

        Command::batch(commands)
    }

    fn focus_next(&mut self) {
        self.focused = (self.focused + 1) % self.launcher_items.len();
    }

    fn focus_previous(&mut self) {
        self.focused = (self.focused + self.launcher_items.len() - 1) % self.launcher_items.len();
    }
}

async fn launch(token: Option<String>, exec: String, gpu: GpuPreference) {
    let mut envs = Vec::new();
    if let Some(token) = token {
        envs.push(("XDG_ACTIVATION_TOKEN".to_string(), token.clone()));
        envs.push(("DESKTOP_STARTUP_ID".to_string(), token));
    }

    if let Some(gpu_envs) = try_get_gpu_envs(gpu).await {
        envs.extend(gpu_envs);
    }

    cosmic::desktop::spawn_desktop_exec(exec, envs);
}

async fn try_get_gpu_envs(gpu: GpuPreference) -> Option<HashMap<String, String>> {
    let connection = zbus::Connection::system().await.ok()?;
    let proxy = switcheroo_control::SwitcherooControlProxy::new(&connection)
        .await
        .ok()?;
    let gpus = proxy.get_gpus().await.ok()?;
    match gpu {
        GpuPreference::Default => gpus.into_iter().find(|gpu| gpu.default),
        GpuPreference::NonDefault => gpus.into_iter().find(|gpu| !gpu.default),
        GpuPreference::SpecificIdx(idx) => gpus.into_iter().nth(idx as usize),
    }
    .map(|gpu| gpu.environment)
}

impl cosmic::Application for CosmicLauncher {
    type Message = Message;
    type Executor = cosmic::executor::single::Executor;
    type Flags = Args;
    const APP_ID: &'static str = "com.system76.CosmicLauncher";

    fn init(mut core: Core, _flags: Args) -> (Self, Command<Message>) {
        core.set_keyboard_nav(false);
        (
            CosmicLauncher {
                core,
                input_value: String::new(),
                surface_state: SurfaceState::Hidden,
                launcher_items: Vec::new(),
                tx: None,
                menu: None,
                cursor_position: None,
                focused: 0,
                last_hide: Instant::now(),
                alt_tab: false,
                queue: VecDeque::new(),
            },
            Command::none(),
        )
    }

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn style(&self) -> Option<<Theme as application::StyleSheet>::Style> {
        Some(<Theme as application::StyleSheet>::Style::Custom(Box::new(
            |theme| cosmic::iced::wayland::Appearance {
                background_color: Color::from_rgba(0.0, 0.0, 0.0, 0.0),
                text_color: theme.cosmic().on_bg_color().into(),
                icon_color: theme.cosmic().on_bg_color().into(),
            },
        )))
    }

    #[allow(clippy::too_many_lines)]
    fn update(&mut self, message: Message) -> Command<Self::Message> {
        match message {
            Message::InputChanged(value) => {
                self.input_value = value.clone();
                self.request(launcher::Request::Search(value));
            }
            Message::Backspace => {
                self.input_value.pop();
                self.request(launcher::Request::Search(self.input_value.clone()));
            }
            Message::TabPress if !self.alt_tab => {
                self.focused = 0;
                return Command::batch(vec![iced::Command::<Id>::widget(find_focused())
                    .map(Message::CompleteFocusedId)
                    .map(cosmic::app::Message::App)]);
            }
            Message::TabPress => {}
            Message::CompleteFocusedId(id) => {
                let i = RESULT_IDS
                    .iter()
                    .position(|res_id| res_id == &id)
                    .unwrap_or_default();

                if let Some(id) = self.launcher_items.get(i).map(|res| res.id) {
                    self.request(launcher::Request::Complete(id));
                }
            }
            Message::Activate(i) => {
                if let Some(item) = self.launcher_items.get(i.unwrap_or(self.focused)) {
                    self.request(launcher::Request::Activate(item.id));
                } else {
                    return self.hide();
                }
            }
            Message::Context(i) => {
                if self.menu.take().is_some() {
                    return commands::popup::destroy_popup(*MENU_ID);
                }

                if let Some(item) = self.launcher_items.get(i) {
                    self.request(launcher::Request::Context(item.id));
                }
            }
            Message::CursorMoved(pos) => {
                self.cursor_position = Some(pos);
            }
            Message::MenuButton(i, context) => {
                self.request(launcher::Request::ActivateContext(i, context));

                if self.menu.take().is_some() {
                    return commands::popup::destroy_popup(*MENU_ID);
                }
            }
            Message::LauncherEvent(e) => match e {
                launcher::Event::Started(tx) => {
                    self.tx.replace(tx);
                }
                launcher::Event::ServiceIsClosed => {
                    self.request(launcher::Request::ServiceIsClosed);
                }
                launcher::Event::Response(response) => match response {
                    pop_launcher::Response::Close => {
                        return self.hide();
                    }
                    #[allow(clippy::cast_possible_truncation)]
                    pop_launcher::Response::Context { id, options } => {
                        if options.is_empty() {
                            return Command::none();
                        }

                        self.menu = Some((id, options));
                        let Some(pos) = self.cursor_position.as_ref() else {
                            return Command::none();
                        };
                        let rect = Rectangle {
                            x: pos.x.round() as i32,
                            y: pos.y.round() as i32,
                            width: 1,
                            height: 1,
                        };

                        return commands::popup::get_popup(SctkPopupSettings {
                            parent: *WINDOW_ID,
                            id: *MENU_ID,
                            positioner: SctkPositioner {
                                size: None,
                                size_limits: Limits::NONE.min_width(1.0).min_height(1.0).max_width(300.0).max_height(800.0),
                                anchor_rect: rect,
                                anchor:
                                    sctk::reexports::protocols::xdg::shell::client::xdg_positioner::Anchor::Right,
                                gravity: sctk::reexports::protocols::xdg::shell::client::xdg_positioner::Gravity::Right,
                                reactive: true,
                                ..Default::default()
                            },
                            grab: true,
                            parent_size: None,
                        });
                    }
                    pop_launcher::Response::DesktopEntry {
                        path,
                        gpu_preference,
                        action_name,
                    } => {
                        if let Some(entry) = cosmic::desktop::load_desktop_file(None, path) {
                            let exec = if let Some(action_name) = action_name {
                                entry
                                    .desktop_actions
                                    .into_iter()
                                    .find(|action| action.name == action_name)
                                    .map(|action| action.exec)
                            } else {
                                entry.exec
                            };

                            let Some(exec) = exec else {
                                return Command::none();
                            };

                            return request_token(
                                Some(String::from(Self::APP_ID)),
                                Some(*WINDOW_ID),
                                move |token| {
                                    cosmic::app::Message::App(Message::ActivationToken(
                                        token,
                                        exec,
                                        gpu_preference,
                                    ))
                                },
                            );
                        }
                    }
                    pop_launcher::Response::Update(mut list) => {
                        if self.alt_tab && list.is_empty() {
                            return self.hide();
                        }
                        list.sort_by(|a, b| {
                            let a = i32::from(a.window.is_none());
                            let b = i32::from(b.window.is_none());
                            a.cmp(&b)
                        });
                        list.truncate(10);
                        self.launcher_items.splice(.., list);

                        let mut cmds = Vec::new();

                        while let Some(element) = self.queue.pop_front() {
                            let updated = self.update(element);
                            cmds.push(updated);
                        }

                        if self.surface_state == SurfaceState::WaitingToBeShown {
                            cmds.push(self.show());
                        }
                        return Command::batch(cmds);
                    }
                    pop_launcher::Response::Fill(s) => {
                        self.input_value = s;
                        self.request(launcher::Request::Search(self.input_value.clone()));
                    }
                },
            },
            Message::Layer(e) => match e {
                LayerEvent::Focused | LayerEvent::Done => {}
                LayerEvent::Unfocused => {
                    self.last_hide = Instant::now();
                    return self.hide();
                }
            },
            Message::CloseContextMenu => {
                if self.menu.take().is_some() {
                    return commands::popup::destroy_popup(*MENU_ID);
                }
            }
            Message::Hide => {
                if self.menu.take().is_some() {
                    return commands::popup::destroy_popup(*MENU_ID);
                }
                return self.hide();
            }
            Message::KeyboardNav(e) => {
                match e {
                    keyboard_nav::Message::FocusNext => {
                        self.focus_next();
                    }
                    keyboard_nav::Message::FocusPrevious => {
                        self.focus_previous();
                    }
                    keyboard_nav::Message::Unfocus => {
                        self.input_value.clear();
                        self.request(launcher::Request::Search(String::new()));
                        return keyboard_nav::unfocus();
                    }
                    _ => {}
                };
            }
            Message::ActivationToken(token, exec, dgpu) => {
                return Command::perform(launch(token, exec, dgpu), |()| {
                    cosmic::app::message::app(Message::Hide)
                });
            }
            Message::AltTab => {
                self.focus_next();
            }
            Message::ShiftAltTab => {
                self.focus_previous();
            }
            Message::AltRelease => {
                if self.alt_tab {
                    return self.update(Message::Activate(None));
                }
            }
        }
        Command::none()
    }

    fn dbus_activation(
        &mut self,
        msg: cosmic::app::DbusActivationMessage,
    ) -> iced::Command<cosmic::app::Message<Self::Message>> {
        match msg.msg {
            DbusActivationDetails::Activate => {
                if self.surface_state != SurfaceState::Hidden {
                    return self.hide();
                }
                // hack: allow to close the launcher from the panel button
                if self.last_hide.elapsed().as_millis() > 100 {
                    self.request(launcher::Request::Search(String::new()));

                    self.surface_state = SurfaceState::WaitingToBeShown;
                    return Command::none();
                }
            }
            DbusActivationDetails::ActivateAction { action, .. } => {
                debug!("ActivateAction {}", action);

                let Ok(cmd) = LauncherCommands::from_str(&action) else {
                    return Command::none();
                };

                if self.surface_state == SurfaceState::Hidden {
                    self.surface_state = SurfaceState::WaitingToBeShown;
                }

                match cmd {
                    LauncherCommands::AltTab => {
                        if self.alt_tab {
                            return self.update(Message::AltTab);
                        }

                        self.alt_tab = true;
                        self.request(launcher::Request::Search(String::new()));
                        self.queue.push_back(Message::AltTab);
                    }
                    LauncherCommands::ShiftAltTab => {
                        if self.alt_tab {
                            return self.update(Message::ShiftAltTab);
                        }

                        self.alt_tab = true;
                        self.request(launcher::Request::Search(String::new()));
                        self.queue.push_back(Message::ShiftAltTab);
                    }
                }
            }
            DbusActivationDetails::Open { .. } => {}
        }
        Command::none()
    }

    fn view(&self) -> Element<Self::Message> {
        unimplemented!()
    }

    #[allow(clippy::too_many_lines)]
    fn view_window(&self, id: SurfaceId) -> Element<Self::Message> {
        if id == *WINDOW_ID {
            let launcher_entry = text_input::search_input(
                "Type to search apps or type “?” for more options...",
                &self.input_value,
            )
            .on_input(Message::InputChanged)
            .on_paste(Message::InputChanged)
            .on_submit(Message::Activate(None))
            .style(cosmic::theme::TextInput::Custom {
                active: Box::new(|theme| theme.focused(&cosmic::theme::TextInput::Search)),
                error: Box::new(|theme| theme.focused(&cosmic::theme::TextInput::Search)),
                hovered: Box::new(|theme| theme.focused(&cosmic::theme::TextInput::Search)),
                focused: Box::new(|theme| theme.focused(&cosmic::theme::TextInput::Search)),
                disabled: Box::new(|theme| theme.disabled(&cosmic::theme::TextInput::Search)),
            })
            .id(INPUT_ID.clone())
            .always_active();

            let buttons: Vec<_> = self
                .launcher_items
                .iter()
                .enumerate()
                .flat_map(|(i, item)| {
                    let (name, desc) = if item.window.is_some() {
                        (&item.description, &item.name)
                    } else {
                        (&item.name, &item.description)
                    };

                    let name = Column::with_children(name.lines().map(|line| {
                        text(if line.width() > 45 {
                            format!("{}...", line.unicode_truncate(45).0)
                        } else {
                            line.to_string()
                        })
                        .horizontal_alignment(Horizontal::Left)
                        .vertical_alignment(Vertical::Center)
                        .size(14)
                        .style(cosmic::theme::Text::Custom(|t| {
                            cosmic::iced::widget::text::Appearance {
                                color: Some(t.cosmic().on_bg_color().into()),
                            }
                        }))
                        .into()
                    }));

                    let desc = Column::with_children(desc.lines().map(|line| {
                        text(if line.width() > 60 {
                            format!("{}...", line.unicode_truncate(60).0)
                        } else {
                            line.to_string()
                        })
                        .horizontal_alignment(Horizontal::Left)
                        .vertical_alignment(Vertical::Center)
                        .size(10)
                        .style(theme::Text::Custom(|t| {
                            cosmic::iced::widget::text::Appearance {
                                color: Some(t.cosmic().on_bg_color().into()),
                            }
                        }))
                        .into()
                    }));

                    let mut button_content = Vec::new();
                    if !self.alt_tab {
                        if let Some(source) = item.category_icon.as_ref() {
                            let name = match source {
                                IconSource::Name(name) | IconSource::Mime(name) => name,
                            };
                            button_content.push(
                                icon(from_name(name.clone()).into())
                                    .width(Length::Fixed(16.0))
                                    .height(Length::Fixed(16.0))
                                    .style(cosmic::theme::Svg::Custom(Rc::new(|theme| {
                                        cosmic::iced_style::svg::Appearance {
                                            color: Some(theme.cosmic().on_bg_color().into()),
                                        }
                                    })))
                                    .into(),
                            );
                        }
                    }
                    if let Some(source) = item.icon.as_ref() {
                        let name = match source {
                            IconSource::Name(name) | IconSource::Mime(name) => name,
                        };
                        button_content.push(
                            icon(
                                from_name(name.clone())
                                    .size(64)
                                    .fallback(Some(IconFallback::Names(vec![
                                        "application-default".into(),
                                        "application-x-executable".into(),
                                    ])))
                                    .into(),
                            )
                            .width(Length::Fixed(32.0))
                            .height(Length::Fixed(32.0))
                            .into(),
                        );
                    }

                    button_content.push(column![name, desc].into());
                    button_content.push(
                        container(
                            text(format!("Ctrl + {}", (i + 1) % 10))
                                .size(14)
                                .vertical_alignment(Vertical::Center)
                                .horizontal_alignment(Horizontal::Right)
                                .style(theme::Text::Custom(|t| {
                                    cosmic::iced::widget::text::Appearance {
                                        color: Some(t.cosmic().on_bg_color().into()),
                                    }
                                })),
                        )
                        .width(Length::Fill)
                        .center_y()
                        .align_y(Vertical::Center)
                        .align_x(Horizontal::Right)
                        .padding([8, 16])
                        .into(),
                    );
                    let is_focused = i == self.focused;
                    let btn = mouse_area(
                        cosmic::widget::button(
                            row(button_content)
                                .spacing(8)
                                .align_items(Alignment::Center),
                        )
                        .id(RESULT_IDS[i].clone())
                        .width(Length::Fill)
                        .on_press(Message::Activate(Some(i)))
                        .padding([8, 16])
                        .style(Button::Custom {
                            active: Box::new(move |focused, theme| {
                                let focused = is_focused || focused;
                                let rad_s = theme.cosmic().corner_radii.radius_s;
                                let a = if focused {
                                    button::StyleSheet::hovered(
                                        theme,
                                        focused,
                                        focused,
                                        &Button::Text,
                                    )
                                } else {
                                    button::StyleSheet::active(
                                        theme,
                                        focused,
                                        focused,
                                        &Button::Text,
                                    )
                                };
                                button::Appearance {
                                    border_radius: rad_s.into(),
                                    outline_width: 0.0,
                                    ..a
                                }
                            }),
                            hovered: Box::new(move |focused, theme| {
                                let focused = is_focused || focused;
                                let rad_s = theme.cosmic().corner_radii.radius_s;

                                let text = button::StyleSheet::hovered(
                                    theme,
                                    focused,
                                    focused,
                                    &Button::Text,
                                );
                                button::Appearance {
                                    border_radius: rad_s.into(),
                                    outline_width: 0.0,
                                    ..text
                                }
                            }),
                            disabled: Box::new(|theme| {
                                let rad_s = theme.cosmic().corner_radii.radius_s;

                                let text = button::StyleSheet::disabled(theme, &Button::Text);
                                button::Appearance {
                                    border_radius: rad_s.into(),
                                    outline_width: 0.0,
                                    ..text
                                }
                            }),
                            pressed: Box::new(move |focused, theme| {
                                let focused = is_focused || focused;
                                let rad_s = theme.cosmic().corner_radii.radius_s;

                                let text = button::StyleSheet::pressed(
                                    theme,
                                    focused,
                                    focused,
                                    &Button::Text,
                                );
                                button::Appearance {
                                    border_radius: rad_s.into(),
                                    outline_width: 0.0,
                                    ..text
                                }
                            }),
                        }),
                    )
                    .on_right_release(Message::Context(i));
                    if i == self.launcher_items.len() - 1 {
                        vec![btn.into()]
                    } else {
                        vec![btn.into(), divider::horizontal::light().into()]
                    }
                })
                .collect();

            let mut content = if self.alt_tab {
                Column::new().max_width(600).spacing(16)
            } else {
                column![launcher_entry].max_width(600).spacing(16)
            };

            if !buttons.is_empty() {
                content = content.push(components::list::column(buttons));
            }

            let window = container(content)
                .style(Container::Custom(Box::new(|theme| container::Appearance {
                    text_color: Some(theme.cosmic().on_bg_color().into()),
                    icon_color: Some(theme.cosmic().on_bg_color().into()),
                    background: Some(Color::from(theme.cosmic().background.base).into()),
                    border: Border {
                        radius: theme.cosmic().corner_radii.radius_m.into(),
                        width: 1.0,
                        color: theme.cosmic().bg_divider().into(),
                    },
                    shadow: Shadow::default(),
                })))
                .padding([24, 32]);

            return if self.menu.is_some() {
                mouse_area(window)
                    .on_release(Message::CloseContextMenu)
                    .on_right_release(Message::CloseContextMenu)
                    .into()
            } else {
                window.into()
            };
        }

        if id == *MENU_ID {
            let Some((i, options)) = self.menu.as_ref() else {
                return container(horizontal_space(Length::Fixed(1.0)))
                    .width(Length::Fixed(1.0))
                    .height(Length::Fixed(1.0))
                    .into();
            };
            let list_column = Column::with_children(options.iter().map(|option| {
                menu_button(text::body(&option.name))
                    .on_press(Message::MenuButton(*i, option.id))
                    .into()
            }))
            .padding([8, 0]);

            return container(
                container(scrollable(list_column)).style(theme::Container::custom(|theme| {
                    let cosmic = theme.cosmic();
                    let corners = cosmic.corner_radii;
                    ContainerAppearance {
                        text_color: Some(cosmic.background.on.into()),
                        background: Some(Color::from(cosmic.background.base).into()),
                        border: Border {
                            radius: corners.radius_m.into(),
                            width: 1.0,
                            color: cosmic.background.divider.into(),
                        },
                        shadow: Shadow::default(),
                        icon_color: Some(cosmic.background.on.into()),
                    }
                })),
            )
            .width(Length::Shrink)
            .height(Length::Shrink)
            .align_x(Horizontal::Center)
            .align_y(Vertical::Top)
            .into();
        }

        vertical_space(Length::Fixed(1.0)).into()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        Subscription::batch(vec![
            launcher::subscription(0).map(Message::LauncherEvent),
            listen_raw(|e, status| match e {
                cosmic::iced::Event::PlatformSpecific(PlatformSpecific::Wayland(
                    wayland::Event::Layer(e, ..),
                )) => Some(Message::Layer(e)),
                cosmic::iced::Event::Keyboard(iced::keyboard::Event::KeyReleased {
                    key, ..
                }) => match key {
                    Key::Named(Named::Alt | Named::Super) => Some(Message::AltRelease),
                    _ => None,
                },
                cosmic::iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                    key,
                    text: _,
                    modifiers,
                    ..
                }) => match key {
                    Key::Character(c) if modifiers.control() && (c == "p" || c == "k") => {
                        Some(Message::KeyboardNav(keyboard_nav::Message::FocusPrevious))
                    }
                    Key::Character(c) if modifiers.control() && (c == "n" || c == "j") => {
                        Some(Message::KeyboardNav(keyboard_nav::Message::FocusNext))
                    }
                    Key::Character(c) if modifiers.control() => {
                        let nums = (0..10)
                            .map(|n| (n.to_string(), ((n + 10) % 10) - 1))
                            .collect::<Vec<_>>();
                        nums.iter()
                            .find_map(|n| (n.0 == c).then(|| Message::Activate(Some(n.1))))
                    }
                    Key::Named(Named::ArrowUp) => {
                        Some(Message::KeyboardNav(keyboard_nav::Message::FocusPrevious))
                    }
                    Key::Named(Named::ArrowDown) => {
                        Some(Message::KeyboardNav(keyboard_nav::Message::FocusNext))
                    }
                    Key::Named(Named::Escape) => Some(Message::Hide),
                    Key::Named(Named::Tab) => Some(Message::TabPress),
                    Key::Named(Named::Backspace)
                        if matches!(status, Status::Ignored) && modifiers.is_empty() =>
                    {
                        Some(Message::Backspace)
                    }
                    _ => None,
                },
                cosmic::iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                    Some(Message::CursorMoved(position))
                }
                _ => None,
            }),
        ])
    }
}

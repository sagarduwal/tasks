pub mod actions;
pub mod context;
pub mod dialog;
pub mod error;
mod flags;
pub mod markdown;
pub mod menu;

pub use flags::*;
use std::{
    any::TypeId,
    collections::{HashMap, VecDeque},
    env, process,
};

use cli_clipboard::{ClipboardContext, ClipboardProvider};
use cosmic::{
    app::{self, Core},
    cosmic_config::{self, Update},
    cosmic_theme::{self, ThemeMode},
    iced::{
        keyboard::{Event as KeyEvent, Modifiers},
        Event, Subscription, Length,
    },
    widget::{
        self,
        calendar::CalendarModel,
        menu::{key_bind::KeyBind, Action as _},
        segmented_button::{Entity, EntityMut, SingleSelect},
    },
    Application, ApplicationExt, Element,
};
use keyring::Entry;

use crate::{
    app::{
        actions::{Action, ApplicationAction, NavMenuAction, TasksAction},
        context::ContextPage,
        dialog::{DialogAction, DialogPage},
    },
    core::{
        config::{self, CONFIG_VERSION},
        icons,
        key_bind::key_binds,
    },
    fl,
    pages::{
        content::{self, Content},
        details::{self, Details},
    },
    storage::{models::List, LocalStorage},
};

pub struct Tasks {
    core: Core,
    about: widget::about::About,
    nav_model: widget::segmented_button::SingleSelectModel,
    storage: LocalStorage,
    content: Content,
    details: Details,
    config_handler: Option<cosmic_config::Config>,
    config: config::TasksConfig,
    app_themes: Vec<String>,
    context_page: ContextPage,
    key_binds: HashMap<KeyBind, Action>,
    modifiers: Modifiers,
    dialog_pages: VecDeque<DialogPage>,
    dialog_text_input: widget::Id,
    
    llm_provider_options: Vec<String>,
    llm_selected_provider: usize,
    llm_model_options: Vec<String>,
    llm_selected_model: usize,
    llm_api_base: String,
    llm_api_key: String,
    llm_testing: bool,
    llm_test_status: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Content(content::Message),
    Details(details::Message),
    Tasks(TasksAction),
    Application(ApplicationAction),
    Open(String),
    Preferences(PreferencesMessage),
}

#[derive(Debug, Clone)]
pub enum PreferencesMessage {
    SetProvider(usize),
    SetModelIdx(usize),
    SetApiBase(String),
    SetApiKey(String),
    Save,
    Test,
}

impl Tasks {
    fn settings(&self) -> Element<Message> {
        let appearance = widget::settings::section()
            .title(fl!("appearance"))
            .add(widget::settings::item::item(
                fl!("theme"),
                widget::dropdown(
                    &self.app_themes,
                    Some(self.config.app_theme.into()),
                    |theme| Message::Application(ApplicationAction::AppTheme(theme)),
                ),
            ));

        let ai_section = widget::settings::section()
            .title(String::from("AI"))
            .add(widget::settings::item::item(
                String::from("Provider"),
                widget::dropdown(
                    &self.llm_provider_options,
                    Some(self.llm_selected_provider),
                    |idx| Message::Preferences(PreferencesMessage::SetProvider(idx)),
                ),
            ))
            .add(widget::settings::item::item(
                String::from("Model"),
                widget::dropdown(
                    &self.llm_model_options,
                    Some(self.llm_selected_model),
                    |idx| Message::Preferences(PreferencesMessage::SetModelIdx(idx)),
                ),
            ))
            .add(widget::settings::item::item(
                String::from("API Base URL"),
                widget::text_input(String::from("https://api.openai.com"), &self.llm_api_base)
                    .on_input(|s| Message::Preferences(PreferencesMessage::SetApiBase(s)))
                    .width(Length::Fixed(420.0)),
            ))
            .add(widget::settings::item::item(
                String::from("API Key"),
                widget::text_input(String::from("••••••••"), &self.llm_api_key)
                    .on_input(|s| Message::Preferences(PreferencesMessage::SetApiKey(s))),
            ))
            .add(
                widget::row()
                    .push(
                        widget::button::standard(String::from("Test Connection"))
                            .on_press_maybe(if self.llm_testing {
                                None
                            } else {
                                Some(Message::Preferences(PreferencesMessage::Test))
                            }),
                    )
                    .push(
                        widget::button::standard(String::from("Save"))
                            .on_press(Message::Preferences(PreferencesMessage::Save)),
                    )
                    .spacing(8),
            )
            .add(
                widget::text::body(
                    if self.llm_testing {
                        String::from("Testing…")
                    } else {
                        self.llm_test_status
                            .as_ref()
                            .cloned()
                            .unwrap_or_else(|| String::from(""))
                    },
                ),
            );

        widget::scrollable(
            widget::column()
                .push(appearance)
                .push(ai_section)
                .spacing(16),
        )
        .into()
    }

    fn create_nav_item(&mut self, list: &List) -> EntityMut<SingleSelect> {
        let icon =
            crate::app::icons::get_icon(list.icon.as_deref().unwrap_or("view-list-symbolic"), 16);
        self.nav_model
            .insert()
            .text(list.name.clone())
            .icon(icon)
            .data(list.clone())
    }

    fn update_content(
        &mut self,
        tasks: &mut Vec<cosmic::Task<cosmic::Action<Message>>>,
        message: content::Message,
    ) {
        let content_tasks = self.content.update(message);
        for content_task in content_tasks {
            match content_task {
                content::Output::Focus(id) => {
                    tasks.push(self.update(Message::Application(ApplicationAction::Focus(id))))
                }
                content::Output::OpenTaskDetails(task) => {
                    let entity = self.details.priority_model.entity_at(task.priority as u16);
                    if let Some(entity) = entity {
                        self.details.priority_model.activate(entity);
                    }
                    self.details.task = task.clone();
                    self.details.text_editor_content =
                        widget::text_editor::Content::with_text(&task.notes);

                    tasks.push(self.update(Message::Application(
                        ApplicationAction::ToggleContextPage(ContextPage::TaskDetails),
                    )));
                }
                content::Output::ToggleHideCompleted(list) => {
                    if let Some(data) = self.nav_model.active_data_mut::<List>() {
                        data.hide_completed = list.hide_completed;
                        if let Err(err) = self.storage.update_list(&list) {
                            tracing::error!("Error updating list: {err}");
                        }
                    }
                }
            }
        }
    }

    fn update_details(
        &mut self,
        tasks: &mut Vec<cosmic::Task<cosmic::Action<Message>>>,
        message: details::Message,
    ) {
        let details_tasks = self.details.update(message);
        for details_task in details_tasks {
            match details_task {
                details::Output::OpenCalendarDialog => {
                    tasks.push(self.update(Message::Application(ApplicationAction::Dialog(
                        DialogAction::Open(DialogPage::Calendar(CalendarModel::now())),
                    ))));
                }
                details::Output::RefreshTask(task) => {
                    tasks.push(self.update(Message::Content(content::Message::RefreshTask(
                        task.clone(),
                    ))));
                }
            }
        }
    }

    fn update_dialog(
        &mut self,
        tasks: &mut Vec<cosmic::Task<cosmic::Action<Message>>>,
        dialog_action: DialogAction,
    ) {
        match dialog_action {
            DialogAction::Open(page) => {
                match page {
                    DialogPage::Rename(entity, _) => {
                        let data = if let Some(entity) = entity {
                            self.nav_model.data::<List>(entity)
                        } else {
                            self.nav_model.active_data::<List>()
                        };
                        if let Some(list) = data {
                            self.dialog_pages
                                .push_back(DialogPage::Rename(entity, list.name.clone()));
                        }
                    }
                    page => self.dialog_pages.push_back(page),
                }
                tasks.push(self.update(Message::Application(ApplicationAction::Focus(
                    self.dialog_text_input.clone(),
                ))));
            }
            DialogAction::Update(dialog_page) => {
                self.dialog_pages[0] = dialog_page;
            }
            DialogAction::Close => {
                self.dialog_pages.pop_front();
            }
            DialogAction::Complete => {
                if let Some(dialog_page) = self.dialog_pages.pop_front() {
                    match dialog_page {
                        DialogPage::New(name) => {
                            let list = List::new(&name);
                            match self.storage.create_list(&list) {
                                Ok(list) => {
                                    tasks.push(
                                        self.update(Message::Tasks(TasksAction::AddList(list))),
                                    );
                                }
                                Err(err) => {
                                    tracing::error!("Error creating list: {err}");
                                }
                            }
                        }
                        DialogPage::Rename(entity, name) => {
                            let data = if let Some(entity) = entity {
                                self.nav_model.data_mut::<List>(entity)
                            } else {
                                self.nav_model.active_data_mut::<List>()
                            };
                            if let Some(list) = data {
                                list.name.clone_from(&name.clone());
                                let list = list.clone();
                                self.nav_model
                                    .text_set(self.nav_model.active(), name.clone());
                                if let Err(err) = self.storage.update_list(&list) {
                                    tracing::error!("Error updating list: {err}");
                                }
                                tasks.push(self.update(Message::Content(
                                    content::Message::SetList(Some(list)),
                                )));
                            }
                        }
                        DialogPage::Delete(entity) => {
                            tasks
                                .push(self.update(Message::Tasks(TasksAction::DeleteList(entity))));
                        }
                        DialogPage::Icon(entity, name, _) => {
                            let data = if let Some(entity) = entity {
                                self.nav_model.data::<List>(entity)
                            } else {
                                self.nav_model.active_data::<List>()
                            };
                            if let Some(list) = data {
                                let entity = self.nav_model.active();
                                self.nav_model.text_set(entity, list.name.clone());
                                self.nav_model
                                    .icon_set(entity, crate::app::icons::get_icon(&name, 16));
                            }
                            if let Some(list) = self.nav_model.active_data_mut::<List>() {
                                list.icon = Some(name);
                                let list = list.clone();
                                if let Err(err) = self.storage.update_list(&list) {
                                    tracing::error!("Error updating list: {err}");
                                }
                                tasks.push(self.update(Message::Content(
                                    content::Message::SetList(Some(list)),
                                )));
                            }
                        }
                        DialogPage::Calendar(date) => {
                            self.details
                                .update(details::Message::SetDueDate(date.selected));
                        }
                        DialogPage::Export(content) => {
                            let Ok(mut clipboard) = ClipboardContext::new() else {
                                tracing::error!("Clipboard is not available");
                                return;
                            };
                            if let Err(error) = clipboard.set_contents(content) {
                                tracing::error!("Error setting clipboard contents: {error}");
                            }
                        }
                    }
                }
            }
            DialogAction::None => (),
        }
    }

    fn update_app(
        &mut self,
        tasks: &mut Vec<cosmic::Task<cosmic::Action<Message>>>,
        application_action: ApplicationAction,
    ) {
        match application_action {
            ApplicationAction::WindowClose => {
                if let Some(window_id) = self.core.main_window_id() {
                    tasks.push(cosmic::iced::window::close(window_id));
                }
            }
            ApplicationAction::WindowNew => match env::current_exe() {
                Ok(exe) => match process::Command::new(&exe).spawn() {
                    Ok(_) => {}
                    Err(err) => {
                        eprintln!("failed to execute {exe:?}: {err}");
                    }
                },
                Err(err) => {
                    eprintln!("failed to get current executable path: {err}");
                }
            },
            ApplicationAction::AppTheme(theme) => {
                if let Some(handler) = &self.config_handler {
                    if let Err(err) = self.config.set_app_theme(handler, theme.into()) {
                        tracing::error!("{err}")
                    }
                }
            }
            ApplicationAction::ToggleHideCompleted(value) => {
                if let Some(handler) = &self.config_handler {
                    if let Err(err) = self.config.set_hide_completed(handler, value) {
                        tracing::error!("{err}")
                    }
                    tasks.push(self.update(Message::Content(content::Message::SetConfig(
                        self.config.clone(),
                    ))));
                }
            }
            ApplicationAction::SystemThemeModeChange => {
                tasks.push(cosmic::command::set_theme(self.config.app_theme.theme()));
            }
            ApplicationAction::Key(modifiers, key) => {
                for (key_bind, action) in self.key_binds.clone().into_iter() {
                    if key_bind.matches(modifiers, &key) {
                        tasks.push(self.update(action.message()));
                    }
                }
            }
            ApplicationAction::Modifiers(modifiers) => {
                self.modifiers = modifiers;
            }
            ApplicationAction::NavMenuAction(nav_menu_action) => match nav_menu_action {
                NavMenuAction::Rename(entity) => {
                    tasks.push(self.update(Message::Application(ApplicationAction::Dialog(
                        DialogAction::Open(DialogPage::Rename(Some(entity), String::new())),
                    ))));
                }
                NavMenuAction::SetIcon(entity) => {
                    tasks.push(self.update(Message::Application(ApplicationAction::Dialog(
                        DialogAction::Open(DialogPage::Icon(
                            Some(entity),
                            String::new(),
                            String::new(),
                        )),
                    ))));
                }
                NavMenuAction::Export(entity) => {
                    if let Some(list) = self.nav_model.data::<List>(entity) {
                        match self.storage.tasks(list) {
                            Ok(data) => {
                                let exported_markdown = LocalStorage::export_list(list, &data);
                                tasks.push(self.update(Message::Application(
                                    ApplicationAction::Dialog(DialogAction::Open(
                                        DialogPage::Export(exported_markdown),
                                    )),
                                )));
                            }
                            Err(err) => {
                                tracing::error!("Error fetching tasks: {err}");
                            }
                        }
                    }
                }
                NavMenuAction::Delete(entity) => {
                    tasks.push(self.update(Message::Application(ApplicationAction::Dialog(
                        DialogAction::Open(DialogPage::Delete(Some(entity))),
                    ))));
                }
            },
            ApplicationAction::ToggleContextPage(context_page) => {
                if self.context_page == context_page {
                    self.core.window.show_context = !self.core.window.show_context;
                } else {
                    self.context_page = context_page;
                    self.core.window.show_context = true;
                }
                tasks.push(
                    self.update(Message::Content(content::Message::ContextMenuOpen(
                        self.core.window.show_context,
                    ))),
                );
            }
            ApplicationAction::ToggleContextDrawer => {
                self.core.window.show_context = !self.core.window.show_context;
                tasks.push(
                    self.update(Message::Content(content::Message::ContextMenuOpen(
                        self.core.window.show_context,
                    ))),
                );
            }
            ApplicationAction::Dialog(dialog_action) => self.update_dialog(tasks, dialog_action),
            ApplicationAction::Focus(id) => tasks.push(widget::text_input::focus(id)),
            ApplicationAction::SortByNameAsc => {
                tasks.push(self.update(Message::Content(content::Message::SetSort(
                    content::SortType::NameAsc,
                ))));
            }
            ApplicationAction::SortByNameDesc => {
                tasks.push(self.update(Message::Content(content::Message::SetSort(
                    content::SortType::NameDesc,
                ))));
            }
            ApplicationAction::SortByDateAsc => {
                tasks.push(self.update(Message::Content(content::Message::SetSort(
                    content::SortType::DateAsc,
                ))));
            }
            ApplicationAction::SortByDateDesc => {
                tasks.push(self.update(Message::Content(content::Message::SetSort(
                    content::SortType::DateDesc,
                ))));
            }
        }
    }

    fn update_tasks(
        &mut self,
        tasks: &mut Vec<cosmic::Task<cosmic::Action<Message>>>,
        tasks_action: TasksAction,
    ) {
        match tasks_action {
            TasksAction::FetchLists => match self.storage.lists() {
                Ok(lists) => {
                    tasks.push(self.update(Message::Tasks(TasksAction::PopulateLists(lists))));
                }
                Err(err) => {
                    tracing::error!("Error fetching lists: {err}");
                }
            },
            TasksAction::PopulateLists(lists) => {
                for list in lists {
                    self.create_nav_item(&list);
                }
                let Some(entity) = self.nav_model.iter().next() else {
                    return;
                };
                self.nav_model.activate(entity);
                let task = self.on_nav_select(entity);
                tasks.push(task);
            }
            TasksAction::AddList(list) => {
                self.create_nav_item(&list);
                let Some(entity) = self.nav_model.iter().last() else {
                    return;
                };
                let task = self.on_nav_select(entity);
                tasks.push(task);
            }
            TasksAction::DeleteList(entity) => {
                let data = if let Some(entity) = entity {
                    self.nav_model.data::<List>(entity)
                } else {
                    self.nav_model.active_data::<List>()
                };
                if let Some(list) = data {
                    if let Err(err) = self.storage.delete_list(list) {
                        tracing::error!("Error deleting list: {err}");
                    }

                    tasks.push(self.update(Message::Content(content::Message::SetList(None))));
                }
                self.nav_model.remove(self.nav_model.active());
            }
        }
    }
}

impl Application for Tasks {
    type Executor = cosmic::executor::Default;
    type Flags = crate::app::Flags;
    type Message = Message;
    const APP_ID: &'static str = "dev.edfloreshz.Tasks";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, flags: Self::Flags) -> (Self, app::Task<Self::Message>) {
        let nav_model = widget::segmented_button::ModelBuilder::default().build();

        let about = widget::about::About::default()
            .name(fl!("tasks"))
            .icon(Self::APP_ID)
            .version("0.2.0")
            .author("Eduardo Flores")
            .license("GPL-3.0-only")
            .links([
                (fl!("repository"), "https://github.com/cosmic-utils/tasks"),
                (
                    fl!("support"),
                    "https://github.com/cosmic-utils/tasks/issues",
                ),
                (fl!("website"), "https://tasks.edfloreshz.dev"),
            ])
            .developers([("Eduardo Flores", "edfloreshz@proton.me")]);

        let mut app = Tasks {
            core,
            about,
            storage: flags.storage.clone(),
            nav_model,
            content: Content::new(flags.storage.clone()),
            details: Details::new(flags.storage),
            config_handler: flags.config_handler,
            config: flags.config,
            app_themes: vec![fl!("match-desktop"), fl!("dark"), fl!("light")],
            context_page: ContextPage::Settings,
            key_binds: key_binds(),
            modifiers: Modifiers::empty(),
            dialog_pages: VecDeque::new(),
            dialog_text_input: widget::Id::unique(),
            llm_provider_options: vec!["OpenAI".into()],
            llm_selected_provider: 0,
            llm_model_options: vec![
                "gpt-4o-mini".into(),
                "gpt-4o".into(),
                "gpt-4.1".into(),
                "gpt-5-mini".into(),
                "gpt-5".into(),
                "o4-mini".into(),
                "o3-mini-high".into(),
            ],
            llm_selected_model: 0,
            llm_api_base: String::from("https://api.openai.com"),
            llm_api_key: String::new(),
            llm_testing: false,
            llm_test_status: None,
        };

        let mut tasks = vec![app.update(Message::Tasks(TasksAction::FetchLists))];

        if let Some(id) = app.core.main_window_id() {
            tasks.push(app.set_window_title(fl!("tasks"), id));
        }

        {
            let provider = if app.config.llm_provider.is_empty() {
                "OpenAI"
            } else {
                &app.config.llm_provider
            };
            if let Some(idx) = app
                .llm_provider_options
                .iter()
                .position(|p| p == provider)
            {
                app.llm_selected_provider = idx;
            }

            let model = if app.config.llm_model.is_empty() {
                "gpt-4o-mini"
            } else {
                &app.config.llm_model
            };
            if let Some(idx) = app.llm_model_options.iter().position(|m| m == model) {
                app.llm_selected_model = idx;
            }

            if !app.config.llm_api_base.is_empty() {
                app.llm_api_base = app.config.llm_api_base.clone();
            }
        }

        {
            if let Ok(entry) = Entry::new(Self::APP_ID, "openai") {
                if let Ok(secret) = entry.get_password() {
                    app.llm_api_key = secret;
                } else if let Ok(env_key) = std::env::var("OPENAI_API_KEY") {
                    app.llm_api_key = env_key;
                }
            }
        }

        app.core.nav_bar_toggle_condensed();

        (app, app::Task::batch(tasks))
    }

    fn context_drawer(&self) -> Option<app::context_drawer::ContextDrawer<Self::Message>> {
        if !self.core.window.show_context {
            return None;
        }

        Some(match self.context_page {
            ContextPage::About => app::context_drawer::about(
                &self.about,
                Message::Open,
                Message::Application(ApplicationAction::ToggleContextDrawer),
            )
            .title(self.context_page.title()),
            ContextPage::Settings => app::context_drawer::context_drawer(
                self.settings(),
                Message::Application(ApplicationAction::ToggleContextDrawer),
            )
            .title(self.context_page.title()),
            ContextPage::TaskDetails => app::context_drawer::context_drawer(
                self.details.view().map(Message::Details),
                Message::Application(ApplicationAction::ToggleContextDrawer),
            )
            .title(self.context_page.title()),
        })
    }

    fn dialog(&self) -> Option<Element<Message>> {
        let dialog_page = self.dialog_pages.front()?;
        let dialog = dialog_page.view(&self.dialog_text_input);
        Some(dialog.into())
    }

    fn header_start(&self) -> Vec<Element<Self::Message>> {
        vec![menu::menu_bar(&self.key_binds, &self.config)]
    }

    fn nav_context_menu(
        &self,
        id: widget::nav_bar::Id,
    ) -> Option<Vec<widget::menu::Tree<cosmic::Action<Self::Message>>>> {
        Some(cosmic::widget::menu::items(
            &HashMap::new(),
            vec![
                cosmic::widget::menu::Item::Button(
                    fl!("rename"),
                    Some(icons::get_handle("edit-symbolic", 14)),
                    NavMenuAction::Rename(id),
                ),
                cosmic::widget::menu::Item::Button(
                    fl!("icon"),
                    Some(icons::get_handle("face-smile-big-symbolic", 14)),
                    NavMenuAction::SetIcon(id),
                ),
                cosmic::widget::menu::Item::Button(
                    fl!("export"),
                    Some(icons::get_handle("share-symbolic", 18)),
                    NavMenuAction::Export(id),
                ),
                cosmic::widget::menu::Item::Button(
                    fl!("delete"),
                    Some(icons::get_handle("user-trash-full-symbolic", 14)),
                    NavMenuAction::Delete(id),
                ),
            ],
        ))
    }

    fn nav_model(&self) -> Option<&widget::segmented_button::SingleSelectModel> {
        Some(&self.nav_model)
    }

    fn on_escape(&mut self) -> app::Task<Self::Message> {
        if self.dialog_pages.pop_front().is_some() {
            return app::Task::none();
        }

        self.core.window.show_context = false;

        app::Task::none()
    }

    fn on_nav_select(&mut self, entity: Entity) -> app::Task<Self::Message> {
        let mut tasks = vec![];
        self.nav_model.activate(entity);
        let location_opt = self.nav_model.data::<List>(entity);

        if let Some(list) = location_opt {
            let message = Message::Content(content::Message::SetList(Some(list.clone())));
            let window_title = format!("{} - {}", list.name, fl!("tasks"));
            if let Some(window_id) = self.core.main_window_id() {
                tasks.push(self.set_window_title(window_title, window_id));
            }
            return self.update(message);
        }

        app::Task::batch(tasks)
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        struct ConfigSubscription;
        struct ThemeSubscription;

        let mut subscriptions = vec![
            cosmic::iced::event::listen_with(|event, _status, _window_id| match event {
                Event::Keyboard(KeyEvent::KeyPressed { key, modifiers, .. }) => {
                    Some(Message::Application(ApplicationAction::Key(modifiers, key)))
                }
                Event::Keyboard(KeyEvent::ModifiersChanged(modifiers)) => Some(
                    Message::Application(ApplicationAction::Modifiers(modifiers)),
                ),
                _ => None,
            }),
            cosmic_config::config_subscription(
                TypeId::of::<ConfigSubscription>(),
                Self::APP_ID.into(),
                CONFIG_VERSION,
            )
            .map(|update: Update<ThemeMode>| {
                if !update.errors.is_empty() {
                    tracing::info!(
                        "errors loading config {:?}: {:?}",
                        update.keys,
                        update.errors
                    );
                }
                Message::Application(ApplicationAction::SystemThemeModeChange)
            }),
            cosmic_config::config_subscription::<_, cosmic_theme::ThemeMode>(
                TypeId::of::<ThemeSubscription>(),
                cosmic_theme::THEME_MODE_ID.into(),
                cosmic_theme::ThemeMode::version(),
            )
            .map(|update: Update<ThemeMode>| {
                if !update.errors.is_empty() {
                    tracing::info!(
                        "errors loading theme mode {:?}: {:?}",
                        update.keys,
                        update.errors
                    );
                }
                Message::Application(ApplicationAction::SystemThemeModeChange)
            }),
        ];

        subscriptions.push(self.content.subscription().map(Message::Content));

        Subscription::batch(subscriptions)
    }

    fn update(&mut self, message: Self::Message) -> app::Task<Self::Message> {
        let mut tasks = vec![];
        match message {
            Message::Open(url) => {
                if let Err(err) = open::that_detached(url) {
                    tracing::error!("{err}")
                }
            }
            Message::Content(message) => {
                self.update_content(&mut tasks, message);
            }
            Message::Details(message) => {
                self.update_details(&mut tasks, message);
            }
            Message::Tasks(tasks_action) => {
                self.update_tasks(&mut tasks, tasks_action);
            }
            Message::Application(application_action) => {
                self.update_app(&mut tasks, application_action);
            }
            Message::Preferences(pref_msg) => {
                match pref_msg {
                    PreferencesMessage::SetProvider(idx) => {
                        self.llm_selected_provider = idx;
                    }
                    PreferencesMessage::SetModelIdx(idx) => {
                        self.llm_selected_model = idx;
                    }
                    PreferencesMessage::SetApiBase(s) => {
                        self.llm_api_base = s;
                    }
                    PreferencesMessage::SetApiKey(s) => {
                        self.llm_api_key = s;
                    }
                    PreferencesMessage::Save => {
                        if let Some(handler) = &self.config_handler {
                            let provider = self
                                .llm_provider_options
                                .get(self.llm_selected_provider)
                                .cloned()
                                .unwrap_or_else(|| String::from("OpenAI"));
                            let model = self
                                .llm_model_options
                                .get(self.llm_selected_model)
                                .cloned()
                                .unwrap_or_else(|| String::from("gpt-4o-mini"));

                            if let Err(err) = self.config.set_llm_provider(handler, provider) {
                                tracing::error!("save llm_provider: {err}");
                            }
                            if let Err(err) = self.config.set_llm_model(handler, model) {
                                tracing::error!("save llm_model: {err}");
                            }
                            if let Err(err) =
                                self.config.set_llm_api_base(handler, self.llm_api_base.clone())
                            {
                                tracing::error!("save llm_api_base: {err}");
                            }
                        }

                        match Entry::new(Self::APP_ID, "openai") {
                            Ok(entry) => {
                                let key = self.llm_api_key.trim();
                                if key.is_empty() {
                                    // Fallback: set an empty password to effectively clear the key.
                                    if let Err(err) = entry.set_password("") {
                                        tracing::warn!("keyring clear failed: {err}");
                                    }
                                } else if let Err(err) = entry.set_password(key) {
                                    tracing::error!("keyring save failed: {err}");
                                }
                            }
                            Err(err) => tracing::error!("keyring init failed: {err}"),
                        }

                        self.llm_test_status = Some(String::from("Saved."));
                    }
                    PreferencesMessage::Test => {
                        self.llm_testing = true;
                        self.llm_test_status = None;
                        let base = self.llm_api_base.trim_end_matches('/').to_string();
                        let key_present = !self.llm_api_key.trim().is_empty();
                        if !key_present {
                            self.llm_testing = false;
                            self.llm_test_status = Some(String::from("Provide an API key to test."));
                        } else {
                            let url = format!("{}/v1/models", base);
                            let result = (|| {
                                let client = match reqwest::blocking::Client::builder()
                                    .timeout(std::time::Duration::from_secs(5))
                                    .build()
                                {
                                    Ok(c) => c,
                                    Err(e) => return Err(format!("client error: {}", e)),
                                };
                                let resp = match client
                                    .get(&url)
                                    .bearer_auth(self.llm_api_key.trim())
                                    .send()
                                {
                                    Ok(r) => r,
                                    Err(e) => return Err(format!("request error: {}", e)),
                                };
                                let status = resp.status();
                                let text = resp.text().unwrap_or_default();
                                if status.is_success() {
                                    Ok(String::from("Success: API reachable"))
                                } else {
                                    let snippet = &text.chars().take(160).collect::<String>();
                                    Err(format!("HTTP {}: {}", status.as_u16(), snippet))
                                }
                            })();

                            match result {
                                Ok(msg) => {
                                    self.llm_test_status = Some(msg);
                                }
                                Err(err) => {
                                    self.llm_test_status = Some(format!("Failed: {}", err));
                                }
                            }
                            self.llm_testing = false;
                        }
                    }
                }
            }
        }

        app::Task::batch(tasks)
    }

    fn view(&self) -> Element<Self::Message> {
        self.content.view().map(Message::Content)
    }
}

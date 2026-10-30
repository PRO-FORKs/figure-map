mod auth;
mod drive;
mod location;
mod map;
mod token_store;

use dioxus::prelude::*;
use rust_i18n::t;

rust_i18n::i18n!("locales", fallback = "ko-KR");

fn main() {
    rust_i18n::set_locale("ko-KR");
    launch(App);
}

#[derive(Clone, PartialEq)]
enum LoginState {
    Restoring,
    SignedOut,
    InProgress,
    SignedIn(auth::Session),
    Failed(String),
}

#[component]
fn App() -> Element {
    let mut state = use_signal(|| LoginState::Restoring);

    use_hook(|| {
        spawn(async move {
            match auth::restore().await {
                Ok(Some(session)) => state.set(LoginState::SignedIn(session)),
                Ok(None) => state.set(LoginState::SignedOut),
                Err(e) => state.set(LoginState::Failed(e.to_string())),
            }
        })
    });

    let sign_in = move |_| {
        state.set(LoginState::InProgress);
        spawn(async move {
            match auth::sign_in().await {
                Ok(session) => state.set(LoginState::SignedIn(session)),
                Err(e) => state.set(LoginState::Failed(e.to_string())),
            }
        });
    };

    let sign_out = move |_| {
        spawn(async move {
            match auth::sign_out().await {
                Ok(()) => state.set(LoginState::SignedOut),
                Err(e) => state.set(LoginState::Failed(e.to_string())),
            }
        });
    };

    rsx! {
        main {
            match state() {
                LoginState::SignedIn(session) => rsx! {
                    p { {t!("login.signed_in_as", email = session.account.email).to_string()} }
                    button { onclick: sign_out, {t!("login.sign_out").to_string()} }
                    Projects { token: session.drive_token, owner: session.account.email }
                },
                LoginState::Restoring => rsx! {
                    p { {t!("login.restoring").to_string()} }
                },
                LoginState::InProgress => rsx! {
                    p { {t!("login.finish_in_browser").to_string()} }
                    button { disabled: true, {t!("login.with_google").to_string()} }
                },
                LoginState::SignedOut => rsx! {
                    button { onclick: sign_in, {t!("login.with_google").to_string()} }
                },
                LoginState::Failed(message) => rsx! {
                    p { {t!("login.failed", message = message).to_string()} }
                    button { onclick: sign_in, {t!("common.retry").to_string()} }
                },
            }
        }
    }
}

#[component]
fn Projects(token: auth::DriveToken, owner: String) -> Element {
    let list_token = token.clone();
    let mut projects = use_resource(move || {
        let token = list_token.clone();
        async move { drive::list_projects(&token).await }
    });
    let mut form_open = use_signal(|| false);
    let mut selected = use_signal(|| None::<String>);

    rsx! {
        if form_open() {
            CreateProjectForm {
                token,
                owner,
                on_close: move |created: bool| {
                    form_open.set(false);
                    if created {
                        projects.restart();
                    }
                },
            }
        } else {
            button { onclick: move |_| form_open.set(true), {t!("project.create").to_string()} }
        }
        match &*projects.read() {
            None => rsx! {
                p { {t!("project.loading").to_string()} }
            },
            Some(Ok(list)) if list.is_empty() => rsx! {},
            Some(Ok(list)) => rsx! {
                ul {
                    for project in list {
                        li { key: "{project.drive_root}",
                            button {
                                r#type: "button",
                                aria_pressed: selected().as_ref() == Some(&project.drive_root),
                                onclick: {
                                    let drive_root = project.drive_root.clone();
                                    move |_| selected.set(Some(drive_root.clone()))
                                },
                                {t!("project.list_item", alias = project.alias, id = project.id).to_string()}
                            }
                        }
                    }
                }
                if let Some(drive_root) = selected() {
                    map::CurrentLocationMap { key: "{drive_root}" }
                }
            },
            Some(Err(e)) => rsx! {
                p { {t!("project.load_failed", message = e).to_string()} }
                button { onclick: move |_| projects.restart(), {t!("common.retry").to_string()} }
            },
        }
    }
}

#[component]
fn CreateProjectForm(
    token: auth::DriveToken,
    owner: String,
    on_close: EventHandler<bool>,
) -> Element {
    let mut project_id = use_signal(String::new);
    let mut alias = use_signal(String::new);
    let mut submitting = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    let submit = move |e: FormEvent| {
        e.prevent_default();
        if submitting() {
            return;
        }
        if let Err(e) = drive::validate_new_project(&project_id.read(), &alias.read()) {
            error.set(Some(e.to_string()));
            return;
        }
        submitting.set(true);
        error.set(None);
        let token = token.clone();
        let owner = owner.clone();
        let (id, name) = (project_id(), alias());
        spawn(async move {
            let result = drive::create_project(&token, &owner, &id, &name).await;
            submitting.set(false);
            match result {
                Ok(_) => on_close.call(true),
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    rsx! {
        form { onsubmit: submit,
            label {
                {t!("project.id_label").to_string()}
                input {
                    value: "{project_id}",
                    placeholder: t!("project.id_placeholder").to_string(),
                    disabled: submitting(),
                    oninput: move |e| project_id.set(e.value()),
                }
            }
            label {
                {t!("project.alias_label").to_string()}
                input {
                    value: "{alias}",
                    placeholder: t!("project.alias_placeholder").to_string(),
                    disabled: submitting(),
                    oninput: move |e| alias.set(e.value()),
                }
            }
            button { r#type: "submit", disabled: submitting(), {t!("project.submit").to_string()} }
            button {
                r#type: "button",
                disabled: submitting(),
                onclick: move |_| on_close.call(false),
                {t!("common.cancel").to_string()}
            }
            if submitting() {
                p { {t!("project.creating").to_string()} }
            }
            if let Some(message) = error() {
                p { {t!("project.create_failed", message = message).to_string()} }
            }
        }
    }
}

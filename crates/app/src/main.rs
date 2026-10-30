mod auth;
mod drive;
mod token_store;

use dioxus::prelude::*;

fn main() {
    dioxus::launch(App);
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
                    p { "로그인한 계정: {session.account.email}" }
                    button { onclick: sign_out, "로그아웃" }
                    Projects { token: session.drive_token, owner: session.account.email }
                },
                LoginState::Restoring => rsx! {
                    p { "로그인 정보를 확인하는 중..." }
                },
                LoginState::InProgress => rsx! {
                    p { "브라우저에서 로그인을 마쳐 줘." }
                    button { disabled: true, "Google로 로그인" }
                },
                LoginState::SignedOut => rsx! {
                    button { onclick: sign_in, "Google로 로그인" }
                },
                LoginState::Failed(message) => rsx! {
                    p { "로그인 실패: {message}" }
                    button { onclick: sign_in, "다시 시도" }
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
            button { onclick: move |_| form_open.set(true), "프로젝트 생성" }
        }
        match &*projects.read() {
            None => rsx! {
                p { "프로젝트 목록을 불러오는 중..." }
            },
            Some(Ok(list)) if list.is_empty() => rsx! {},
            Some(Ok(list)) => rsx! {
                ul {
                    for project in list {
                        li { key: "{project.drive_root}", "{project.alias} ({project.owner})" }
                    }
                }
            },
            Some(Err(e)) => rsx! {
                p { "프로젝트 목록을 불러오지 못했어: {e}" }
                button { onclick: move |_| projects.restart(), "다시 시도" }
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
                "프로젝트 ID "
                input {
                    value: "{project_id}",
                    placeholder: "dongmyo-2026",
                    disabled: submitting(),
                    oninput: move |e| project_id.set(e.value()),
                }
            }
            label {
                "별명 "
                input {
                    value: "{alias}",
                    placeholder: "동묘 투어",
                    disabled: submitting(),
                    oninput: move |e| alias.set(e.value()),
                }
            }
            button { r#type: "submit", disabled: submitting(), "생성" }
            button {
                r#type: "button",
                disabled: submitting(),
                onclick: move |_| on_close.call(false),
                "취소"
            }
            if submitting() {
                p { "프로젝트를 만드는 중..." }
            }
            if let Some(message) = error() {
                p { "프로젝트를 만들지 못했어: {message}" }
            }
        }
    }
}

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
                    Projects { token: session.drive_token }
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
fn Projects(token: auth::DriveToken) -> Element {
    let mut projects = use_resource(move || {
        let token = token.clone();
        async move { drive::list_projects(&token).await }
    });

    rsx! {
        // TODO: 프로젝트 생성 흐름은 아직 구현하지 않았다.
        button { "프로젝트 생성" }
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

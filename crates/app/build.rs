use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

const PATH_ENV: &str = "GOOGLE_OAUTH_CLIENT_FILE";
const DEFAULT_PATH: &str = "../../secrets/google_oauth_client.json";
const MAPS_KEY_PATH_ENV: &str = "GOOGLE_MAPS_API_KEY_FILE";
const MAPS_KEY_DEFAULT_PATH: &str = "../../secrets/google_maps_api_key.txt";

fn main() -> ExitCode {
    embed_maps_key();
    println!("cargo:rerun-if-env-changed={PATH_ENV}");

    let path = match env::var_os(PATH_ENV) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_PATH),
    };
    println!("cargo:rerun-if-changed={}", path.display());

    match read_client(&path) {
        Ok((client_id, client_secret)) => {
            println!("cargo:rustc-env=GOOGLE_OAUTH_CLIENT_ID={client_id}");
            println!("cargo:rustc-env=GOOGLE_OAUTH_CLIENT_SECRET={client_secret}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!(
                "Google OAuth client 파일을 읽을 수 없다: {}\n{message}\n\
                 Google Cloud Console에서 받은 데스크톱 앱 client JSON을 위 경로에 두거나 \
                 {PATH_ENV} 환경 변수로 경로를 지정한다.",
                path.display()
            );
            ExitCode::FAILURE
        }
    }
}

/// 지도 키는 선택 사항이다. 없으면 앱이 지도 대신 안내 문구를 보여준다.
fn embed_maps_key() {
    println!("cargo:rerun-if-env-changed={MAPS_KEY_PATH_ENV}");
    let path = match env::var_os(MAPS_KEY_PATH_ENV) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(MAPS_KEY_DEFAULT_PATH),
    };
    println!("cargo:rerun-if-changed={}", path.display());
    match std::fs::read_to_string(&path) {
        Ok(key) if !key.trim().is_empty() => {
            println!("cargo:rustc-env=GOOGLE_MAPS_API_KEY={}", key.trim());
        }
        _ => println!(
            "cargo:warning=Google Maps API 키가 없다: {}. 지도 없이 빌드한다.",
            path.display()
        ),
    }
}

fn read_client(path: &PathBuf) -> Result<(String, String), String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    // 시스템 브라우저 + loopback redirect 흐름은 "데스크톱 앱" 유형(installed) client만 허용한다.
    let installed = json
        .get("installed")
        .ok_or("\"installed\" 항목이 없다. 데스크톱 앱 유형 client인지 확인한다.")?;
    let field = |name: &str| {
        installed
            .get(name)
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .ok_or(format!("installed.{name} 값이 없다."))
    };
    Ok((field("client_id")?, field("client_secret")?))
}

//! Google refresh token 보관소. macOS는 Keychain을 쓴다.
//! Android 보안 저장소는 아직 연결하지 않아 저장하지 않는다(매번 로그인).

#[derive(Debug, thiserror::Error)]
#[error("보안 저장소 오류: {0}")]
pub struct StoreError(String);

#[cfg(target_os = "macos")]
mod imp {
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };

    use super::StoreError;

    const SERVICE: &str = "shared-map.google-oauth";
    const ACCOUNT: &str = "refresh_token";
    // security-framework-sys의 errSecItemNotFound.
    const ITEM_NOT_FOUND: i32 = -25300;

    pub fn load() -> Result<Option<String>, StoreError> {
        match get_generic_password(SERVICE, ACCOUNT) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| StoreError("저장된 토큰이 UTF-8이 아니야".to_owned())),
            Err(e) if e.code() == ITEM_NOT_FOUND => Ok(None),
            Err(e) => Err(StoreError(e.to_string())),
        }
    }

    pub fn save(token: &str) -> Result<(), StoreError> {
        set_generic_password(SERVICE, ACCOUNT, token.as_bytes())
            .map_err(|e| StoreError(e.to_string()))
    }

    pub fn clear() -> Result<(), StoreError> {
        match delete_generic_password(SERVICE, ACCOUNT) {
            Err(e) if e.code() != ITEM_NOT_FOUND => Err(StoreError(e.to_string())),
            _ => Ok(()),
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::StoreError;

    pub fn load() -> Result<Option<String>, StoreError> {
        Ok(None)
    }

    pub fn save(_token: &str) -> Result<(), StoreError> {
        Ok(())
    }

    pub fn clear() -> Result<(), StoreError> {
        Ok(())
    }
}

// Keychain 접근 허용 대화상자가 뜰 수 있으므로 UI 스레드 밖에서 실행한다.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, StoreError> + Send + 'static,
) -> Result<T, StoreError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| StoreError(e.to_string()))?
}

pub async fn load() -> Result<Option<String>, StoreError> {
    blocking(imp::load).await
}

pub async fn save(token: String) -> Result<(), StoreError> {
    blocking(move || imp::save(&token)).await
}

pub async fn clear() -> Result<(), StoreError> {
    blocking(imp::clear).await
}

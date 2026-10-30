//! 현재 위치 어댑터.
//! macOS의 WKWebView는 wry가 geolocation 권한 요청을 처리하지 않아 navigator.geolocation이 거부되므로 CoreLocation을 직접 쓴다.
//! 다른 타깃은 `Unsupported`를 돌려주고 지도 webview의 navigator.geolocation을 쓴다. Android는 wry가 런타임 권한을 요청한다.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Position {
    pub lat: f64,
    pub lng: f64,
    pub accuracy_m: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocationError {
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Unsupported,
    Denied,
    Unavailable,
}

#[cfg(target_os = "macos")]
pub use macos::current_position;

#[cfg(not(target_os = "macos"))]
pub async fn current_position() -> Result<Position, LocationError> {
    Err(LocationError::Unsupported)
}

#[cfg(target_os = "macos")]
mod macos {
    use std::time::Duration;

    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
    use objc2_core_location::{
        CLAuthorizationStatus, CLError, CLLocation, CLLocationManager, CLLocationManagerDelegate,
        kCLLocationAccuracyBest,
    };
    use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
    use tokio::time::Instant;

    use super::{LocationError, Position};

    // 권한 대화상자에 응답하는 시간은 제한하지 않고, 권한을 얻은 뒤 위치 측정에만 적용한다.
    const FIX_TIMEOUT: Duration = Duration::from_secs(15);

    enum Update {
        Authorization,
        Located(Position),
        Failed(LocationError),
    }

    struct Ivars {
        tx: UnboundedSender<Update>,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "SharedMapLocationDelegate"]
        #[ivars = Ivars]
        struct Delegate;

        unsafe impl NSObjectProtocol for Delegate {}

        unsafe impl CLLocationManagerDelegate for Delegate {
            #[unsafe(method(locationManagerDidChangeAuthorization:))]
            fn did_change_authorization(&self, _manager: &CLLocationManager) {
                self.notify(Update::Authorization);
            }

            #[unsafe(method(locationManager:didUpdateLocations:))]
            fn did_update_locations(
                &self,
                _manager: &CLLocationManager,
                locations: &NSArray<CLLocation>,
            ) {
                let update = locations
                    .lastObject()
                    .and_then(|location| to_position(&location))
                    .map_or(Update::Failed(LocationError::Unavailable), Update::Located);
                self.notify(update);
            }

            #[unsafe(method(locationManager:didFailWithError:))]
            fn did_fail_with_error(&self, _manager: &CLLocationManager, error: &NSError) {
                let error = if error.code() == CLError::Denied.0 {
                    LocationError::Denied
                } else {
                    LocationError::Unavailable
                };
                self.notify(Update::Failed(error));
            }
        }
    );

    impl Delegate {
        fn new(mtm: MainThreadMarker, tx: UnboundedSender<Update>) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(Ivars { tx });
            unsafe { msg_send![super(this), init] }
        }

        fn notify(&self, update: Update) {
            // 수신 쪽이 이미 끝났으면 버린다.
            let _ = self.ivars().tx.send(update);
        }
    }

    fn to_position(location: &CLLocation) -> Option<Position> {
        let accuracy_m = unsafe { location.horizontalAccuracy() };
        // 음수 정확도는 CoreLocation이 좌표가 유효하지 않다고 표시한 것이다.
        if accuracy_m < 0.0 {
            return None;
        }
        let coordinate = unsafe { location.coordinate() };
        Some(Position {
            lat: coordinate.latitude,
            lng: coordinate.longitude,
            accuracy_m,
        })
    }

    /// 필요하면 위치 권한을 요청하고 현재 위치를 한 번 측정한다.
    /// CLLocationManager는 run loop가 있는 스레드가 필요해 Dioxus desktop이 VirtualDom을 돌리는 메인 스레드에서 불러야 한다.
    pub async fn current_position() -> Result<Position, LocationError> {
        let Some(mtm) = MainThreadMarker::new() else {
            return Err(LocationError::Unavailable);
        };
        let (tx, mut rx) = unbounded_channel();
        let delegate = Delegate::new(mtm, tx);
        // manager가 delegate보다 먼저 해제되도록 나중에 선언한다. CoreLocation은 delegate를 약하게 참조한다.
        let manager = unsafe { CLLocationManager::new() };
        unsafe {
            manager.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            manager.setDesiredAccuracy(kCLLocationAccuracyBest);
        }
        let result = wait_for_position(&manager, &mut rx).await;
        unsafe { manager.setDelegate(None) };
        result
    }

    async fn wait_for_position(
        manager: &CLLocationManager,
        rx: &mut UnboundedReceiver<Update>,
    ) -> Result<Position, LocationError> {
        let mut authorization_requested = false;
        let mut deadline = None;
        loop {
            match unsafe { manager.authorizationStatus() } {
                CLAuthorizationStatus::NotDetermined => {
                    if !authorization_requested {
                        authorization_requested = true;
                        unsafe { manager.requestWhenInUseAuthorization() };
                    }
                }
                CLAuthorizationStatus::Denied | CLAuthorizationStatus::Restricted => {
                    return Err(LocationError::Denied);
                }
                _ => {
                    if deadline.is_none() {
                        deadline = Some(Instant::now() + FIX_TIMEOUT);
                        unsafe { manager.requestLocation() };
                    }
                }
            }

            let update = match deadline {
                Some(deadline) => tokio::time::timeout_at(deadline, rx.recv())
                    .await
                    .map_err(|_| LocationError::Unavailable)?,
                None => rx.recv().await,
            };
            match update {
                Some(Update::Authorization) => {}
                Some(Update::Located(position)) => return Ok(position),
                Some(Update::Failed(error)) => return Err(error),
                None => return Err(LocationError::Unavailable),
            }
        }
    }
}

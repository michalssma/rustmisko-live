fn main() {
    use governor::{Quota, RateLimiter};
    use std::num::NonZeroU32;
    use std::time::Duration;
    let _q = Quota::with_period(Duration::from_millis(1250)).unwrap();
    let _limiter = RateLimiter::direct(_q);
}

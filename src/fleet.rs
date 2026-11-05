//! Per-Pill bookkeeping: which Pills we know about, their newest readings, and which are due to
//! be posted to Brewfather. No ESP or BLE dependencies and the clock is passed in, so all of it
//! is tested on the host (see `scripts/host-test.sh`).

use crate::pills::Addr;
use crate::rapt::Reading;
use std::time::{Duration, Instant};

/// Minimum time between successful Brewfather posts for one Pill (its limit is 1 post per device
/// name per 15 minutes).
pub const POST_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// A reading older than this is stale and is not posted.
pub const MAX_READING_AGE: Duration = Duration::from_secs(20 * 60);
/// After a failed post, wait this long before offering that Pill again. Without it a failing
/// network (e.g. Wi-Fi up but no internet) would be retried every main-loop tick.
pub const POST_RETRY_DELAY: Duration = Duration::from_secs(30);

struct PillState {
    addr: Addr,
    /// Brewfather device name.
    name: String,
    /// Newest valid reading and when it was heard; cleared once posted.
    latest: Option<(Reading, Instant)>,
    last_post: Option<Instant>,
    /// Newest reading heard, kept after posting so the settings page can show it.
    last_heard: Option<(Reading, i8, Instant)>,
    /// Whether this Pill has been heard since boot.
    seen: bool,
    /// Do not offer this Pill again before this time (set after a failed post).
    retry_after: Option<Instant>,
}

impl PillState {
    fn new(addr: Addr, name: String) -> Self {
        PillState { addr, name, latest: None, last_heard: None, last_post: None, seen: false, retry_after: None }
    }
}

/// Whether an HTTP response to a Brewfather post counts as success. Only 2xx does: a router's
/// "device paused" page, a captive portal or a rate-limit/error from Brewfather must not be
/// mistaken for a delivered reading (the reading would be dropped and not retried for 15 minutes).
pub fn is_success_status(status: u16) -> bool {
    (200..300).contains(&status)
}

/// What `Fleet::on_heard` made of an advertisement, so the caller can log it.
#[derive(Debug, PartialEq)]
pub enum Heard<'a> {
    /// A configured Pill; its reading was stored. `first` is true the first time it is heard
    /// since boot (the caller logs that; later sightings are routine).
    Listed { name: &'a str, first: bool },
    /// A Pill that is not configured, seen for the first time (worth telling the user about).
    UnlistedNew,
    /// A Pill that is not configured and was already reported.
    UnlistedAgain,
}

/// A reading that should be posted now.
#[derive(Debug, PartialEq)]
pub struct Post {
    /// Pass back to `Fleet::mark_posted` after a successful post.
    pub idx: usize,
    pub name: String,
    pub reading: Reading,
}

/// One decoded advertisement from a RAPT Pill.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sighting {
    pub addr: Addr,
    pub rssi: i8,
    pub reading: Reading,
}

/// Hand-off between the Bluetooth scan thread (producer) and the main thread (consumer).
///
/// The scan callback runs on a tiny task, so it only pushes here and returns. Only the newest
/// sighting per address is kept, which is all `Fleet` wants, so a burst of packets can't grow
/// the queue, and the number of distinct addresses is capped so a flood of spoofed Pills can't
/// either.
#[derive(Default)]
pub struct SightingQueue {
    items: Vec<Sighting>,
}

impl SightingQueue {
    /// Most distinct Pill addresses held at once; sightings from further addresses are dropped
    /// until the queue is drained.
    pub const MAX_PILLS: usize = 16;

    pub fn push(&mut self, s: Sighting) {
        match self.items.iter().position(|i| i.addr == s.addr) {
            Some(i) => self.items[i] = s,
            None if self.items.len() < Self::MAX_PILLS => self.items.push(s),
            None => {}
        }
    }

    /// Take everything queued so far, leaving the queue empty.
    pub fn take(&mut self) -> Vec<Sighting> {
        std::mem::take(&mut self.items)
    }
}

/// A Pill that is heard but not configured: shown on the settings page so the user can name it.
/// Its reading is never posted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UnlistedPill {
    pub addr: Addr,
    pub rssi: i8,
    pub reading: Reading,
    pub age: Duration,
}

/// A configured Pill as shown on the settings page.
#[derive(Debug, Clone, PartialEq)]
pub struct PillStatus {
    pub addr: Addr,
    pub name: String,
    /// Newest reading, its signal strength and how long ago it was heard; `None` if never heard
    /// since boot.
    pub last: Option<(Reading, i8, Duration)>,
}

/// Most distinct unlisted Pills remembered at once (same bound as `SightingQueue`), so a flood of
/// spoofed advertisements can't grow memory.
const MAX_UNLISTED: usize = SightingQueue::MAX_PILLS;

pub struct Fleet {
    pills: Vec<PillState>,
    unlisted: Vec<(Addr, i8, Reading, Instant)>,
}

impl Fleet {
    pub fn new(configured: Vec<(Addr, String)>) -> Self {
        Fleet {
            pills: configured
                .into_iter()
                .map(|(addr, name)| PillState::new(addr, name))
                .collect(),
            unlisted: Vec::new(),
        }
    }

    /// Replace the configured list (the user edited it on the settings page).
    ///
    /// * A Pill that stays listed with the same name keeps its reading and timers.
    /// * A renamed Pill keeps its reading but is due immediately: Brewfather sees a new device
    ///   with its own 15-minute limit.
    /// * A Pill that was only heard as unlisted becomes configured with that reading, so it is
    ///   posted on the next pass instead of waiting for another advertisement.
    /// * Removed Pills are forgotten.
    pub fn set_configured(&mut self, configured: Vec<(Addr, String)>) {
        let mut old = std::mem::take(&mut self.pills);
        for (addr, name) in configured {
            if let Some(i) = old.iter().position(|p| p.addr == addr) {
                let mut p = old.swap_remove(i);
                if p.name != name {
                    p.name = name;
                    p.last_post = None;
                    p.retry_after = None;
                }
                self.pills.push(p);
            } else {
                let mut p = PillState::new(addr, name);
                if let Some(i) = self.unlisted.iter().position(|u| u.0 == addr) {
                    let (_, rssi, reading, heard_at) = self.unlisted.remove(i);
                    p.latest = Some((reading, heard_at));
                    p.last_heard = Some((reading, rssi, heard_at));
                    p.seen = true;
                }
                self.pills.push(p);
            }
        }
        // A removed Pill goes back to being "heard but unlisted" if its reading is still fresh.
        for p in old {
            if let Some((reading, rssi, heard_at)) = p.last_heard {
                self.remember_unlisted(p.addr, rssi, reading, heard_at);
            }
        }
    }

    /// Configured Pills with their newest readings, for the settings page.
    pub fn status(&self, now: Instant) -> Vec<PillStatus> {
        self.pills
            .iter()
            .map(|p| PillStatus {
                addr: p.addr,
                name: p.name.clone(),
                last: p.last_heard.map(|(r, rssi, t)| (r, rssi, now.saturating_duration_since(t))),
            })
            .collect()
    }

    /// Pills heard recently that are not configured, newest first.
    pub fn unlisted(&self, now: Instant) -> Vec<UnlistedPill> {
        let mut out: Vec<UnlistedPill> = self
            .unlisted
            .iter()
            .map(|&(addr, rssi, reading, t)| UnlistedPill {
                addr,
                rssi,
                reading,
                age: now.saturating_duration_since(t),
            })
            .collect();
        out.sort_by_key(|u| u.age);
        out
    }

    fn remember_unlisted(&mut self, addr: Addr, rssi: i8, reading: Reading, now: Instant) -> bool {
        if let Some(u) = self.unlisted.iter_mut().find(|u| u.0 == addr) {
            *u = (addr, rssi, reading, now);
            return false;
        }
        if self.unlisted.len() < MAX_UNLISTED {
            self.unlisted.push((addr, rssi, reading, now));
            return true;
        }
        false
    }

    /// `(address, name)` of each configured Pill, in configuration order.
    pub fn configured(&self) -> impl Iterator<Item = (&Addr, &str)> {
        self.pills.iter().map(|p| (&p.addr, p.name.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.pills.is_empty()
    }

    /// Record a reading heard from `addr`. Unlisted Pills are remembered only for display, so they
    /// can't be posted.
    pub fn on_heard(&mut self, addr: Addr, rssi: i8, reading: Reading, now: Instant) -> Heard<'_> {
        if let Some(i) = self.pills.iter().position(|p| p.addr == addr) {
            let p = &mut self.pills[i];
            p.latest = Some((reading, now));
            p.last_heard = Some((reading, rssi, now));
            let first = !p.seen;
            p.seen = true;
            return Heard::Listed { name: &p.name, first };
        }
        if self.remember_unlisted(addr, rssi, reading, now) {
            Heard::UnlistedNew
        } else {
            Heard::UnlistedAgain
        }
    }

    /// Forget readings older than `MAX_READING_AGE`. Returns the names of the Pills affected.
    pub fn drop_stale(&mut self, now: Instant) -> Vec<String> {
        self.unlisted.retain(|u| now.saturating_duration_since(u.3) <= MAX_READING_AGE);
        let mut dropped = Vec::new();
        for p in &mut self.pills {
            if let Some((_, heard_at)) = p.latest {
                if now.saturating_duration_since(heard_at) > MAX_READING_AGE {
                    p.latest = None;
                    dropped.push(p.name.clone());
                }
            }
        }
        dropped
    }

    /// Readings that should be posted now: a reading is held and `POST_INTERVAL` has passed since
    /// that Pill's last successful post. Each Pill is judged on its own. Nothing is consumed:
    /// call `mark_posted` for each one that actually succeeds; the rest are offered again next time.
    pub fn due(&self, now: Instant) -> Vec<Post> {
        self.pills
            .iter()
            .enumerate()
            .filter_map(|(idx, p)| {
                let (reading, heard_at) = p.latest?;
                let interval_elapsed =
                    p.last_post.map_or(true, |t| now.saturating_duration_since(t) >= POST_INTERVAL);
                let fresh = now.saturating_duration_since(heard_at) <= MAX_READING_AGE;
                let backed_off = p.retry_after.is_some_and(|t| now < t);
                (interval_elapsed && fresh && !backed_off).then(|| Post { idx, name: p.name.clone(), reading })
            })
            .collect()
    }

    pub fn mark_posted(&mut self, idx: usize, now: Instant) {
        let p = &mut self.pills[idx];
        p.last_post = Some(now);
        p.latest = None;
        p.retry_after = None;
    }

    /// Record that posting this Pill failed: it is not offered again for `POST_RETRY_DELAY`.
    /// Its reading is kept (until it goes stale) and other Pills are unaffected.
    pub fn mark_failed(&mut self, idx: usize, now: Instant) {
        self.pills[idx].retry_after = Some(now + POST_RETRY_DELAY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Addr = [0xFC, 0xE8, 0xC0, 0x5D, 0x74, 0x76];
    const B: Addr = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
    const STRANGER: Addr = [1, 2, 3, 4, 5, 6];

    fn reading(sg: f32) -> Reading {
        Reading { temp_c: 20.0, gravity: sg, battery_pct: 80 }
    }

    fn two_pills() -> Fleet {
        Fleet::new(vec![(A, "Yellow Pill 1".into()), (B, "Red Pill 2".into())])
    }

    fn mins(n: u64) -> Duration {
        Duration::from_secs(n * 60)
    }

    #[test]
    fn only_2xx_responses_count_as_a_delivered_post() {
        for ok in [200, 201, 204, 299] {
            assert!(is_success_status(ok), "{ok} should be success");
        }
        // 405 is what a router's "paused device" page answered with; the rest are typical
        // redirects (captive portals), client errors, rate limits and server errors.
        for bad in [0, 100, 199, 300, 302, 400, 401, 403, 404, 405, 429, 500, 502, 503] {
            assert!(!is_success_status(bad), "{bad} must not be success");
        }
    }

    #[test]
    fn nothing_is_due_before_any_reading() {
        let fleet = two_pills();
        assert!(fleet.due(Instant::now()).is_empty());
    }

    #[test]
    fn first_reading_is_posted_immediately_under_its_own_name() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -60, reading(1.050), t0);
        let due = fleet.due(t0);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].name, "Yellow Pill 1");
        assert_eq!(due[0].reading.gravity, 1.050);
    }

    #[test]
    fn each_pill_is_reported_under_its_own_name_with_its_own_reading() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -60, reading(1.050), t0);
        fleet.on_heard(B, -60, reading(1.012), t0);
        let due = fleet.due(t0);
        assert_eq!(due.len(), 2);
        let a = due.iter().find(|p| p.name == "Yellow Pill 1").unwrap();
        let b = due.iter().find(|p| p.name == "Red Pill 2").unwrap();
        assert_eq!(a.reading.gravity, 1.050);
        assert_eq!(b.reading.gravity, 1.012);
    }

    #[test]
    fn post_timers_are_independent_per_pill() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -60, reading(1.050), t0);
        fleet.on_heard(B, -60, reading(1.012), t0);

        // Only A posts successfully at t0.
        let due = fleet.due(t0);
        let a_idx = due.iter().find(|p| p.name == "Yellow Pill 1").unwrap().idx;
        fleet.mark_posted(a_idx, t0);

        // Five minutes later both Pills report again. A is inside its 15 minutes, B never posted.
        let t5 = t0 + mins(5);
        fleet.on_heard(A, -60, reading(1.049), t5);
        fleet.on_heard(B, -60, reading(1.011), t5);
        let due = fleet.due(t5);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].name, "Red Pill 2");

        // At 15 minutes A is due again as well.
        let t15 = t0 + mins(15);
        fleet.on_heard(A, -60, reading(1.048), t15);
        let names: Vec<_> = fleet.due(t15).into_iter().map(|p| p.name).collect();
        assert!(names.contains(&"Yellow Pill 1".to_string()));
    }

    #[test]
    fn interval_is_measured_from_the_last_successful_post() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -60, reading(1.050), t0);
        fleet.mark_posted(0, t0);

        fleet.on_heard(A, -60, reading(1.049), t0 + mins(14));
        assert!(fleet.due(t0 + mins(14)).is_empty());
        fleet.on_heard(A, -60, reading(1.048), t0 + mins(15));
        assert_eq!(fleet.due(t0 + mins(15)).len(), 1);
    }

    #[test]
    fn a_failed_post_is_offered_again_and_does_not_block_other_pills() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -60, reading(1.050), t0);
        fleet.on_heard(B, -60, reading(1.012), t0);

        // A's post fails (never marked); B's succeeds.
        let due = fleet.due(t0);
        let b_idx = due.iter().find(|p| p.name == "Red Pill 2").unwrap().idx;
        fleet.mark_posted(b_idx, t0);

        let later = t0 + Duration::from_secs(30);
        let due = fleet.due(later);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].name, "Yellow Pill 1");
        assert_eq!(due[0].reading.gravity, 1.050);
    }

    #[test]
    fn a_failed_post_is_not_retried_until_the_delay_has_passed() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -60, reading(1.050), t0);
        assert_eq!(fleet.due(t0).len(), 1);
        fleet.mark_failed(0, t0);

        // Every one-second tick inside the delay is quiet...
        for secs in [0, 1, 2, 15, 29] {
            let t = t0 + Duration::from_secs(secs);
            fleet.on_heard(A, -60, reading(1.049), t); // a fresh reading must not bypass the backoff
            assert!(fleet.due(t).is_empty(), "offered again after only {secs}s");
        }
        // ...and it is offered again, with the newest reading, once the delay is over.
        let t30 = t0 + POST_RETRY_DELAY;
        fleet.on_heard(A, -60, reading(1.048), t30);
        let due = fleet.due(t30);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].reading.gravity, 1.048);
    }

    #[test]
    fn repeated_failures_keep_backing_off_without_giving_up() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -60, reading(1.050), t0);
        let mut now = t0;
        for _ in 0..5 {
            assert_eq!(fleet.due(now).len(), 1);
            fleet.mark_failed(0, now);
            assert!(fleet.due(now + Duration::from_secs(1)).is_empty());
            now += POST_RETRY_DELAY;
            fleet.on_heard(A, -60, reading(1.050), now); // keep the reading fresh
        }
    }

    #[test]
    fn one_pills_backoff_does_not_delay_another() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -60, reading(1.050), t0);
        fleet.on_heard(B, -60, reading(1.012), t0);
        fleet.mark_failed(0, t0); // A fails
        let due = fleet.due(t0 + Duration::from_secs(1));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].name, "Red Pill 2");
    }

    #[test]
    fn backoff_does_not_keep_a_stale_reading_alive() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -60, reading(1.050), t0);
        fleet.mark_failed(0, t0);
        let late = t0 + mins(21);
        assert_eq!(fleet.drop_stale(late), vec!["Yellow Pill 1".to_string()]);
        assert!(fleet.due(late).is_empty());
    }

    #[test]
    fn a_successful_post_after_failures_resets_the_normal_cycle() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -60, reading(1.050), t0);
        fleet.mark_failed(0, t0);
        let t31 = t0 + Duration::from_secs(31);
        fleet.mark_posted(0, t31);
        // Next post is 15 minutes after the success, not affected by the earlier failure.
        fleet.on_heard(A, -60, reading(1.049), t31 + mins(14));
        assert!(fleet.due(t31 + mins(14)).is_empty());
        fleet.on_heard(A, -60, reading(1.048), t31 + mins(15));
        assert_eq!(fleet.due(t31 + mins(15)).len(), 1);
    }

    #[test]
    fn posted_reading_is_not_posted_twice() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -60, reading(1.050), t0);
        fleet.mark_posted(0, t0);
        assert!(fleet.due(t0 + mins(30)).is_empty());
    }

    #[test]
    fn old_reading_is_not_reposted_when_the_interval_passes_without_a_new_one() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -60, reading(1.050), t0);
        fleet.mark_posted(0, t0);
        // 15 minutes later the interval has elapsed and the old reading is not yet stale,
        // but it was already sent, so it must not be sent again.
        assert!(fleet.due(t0 + mins(15)).is_empty());
        assert!(fleet.due(t0 + mins(16)).is_empty());
    }

    #[test]
    fn newest_reading_wins() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -60, reading(1.050), t0);
        fleet.on_heard(A, -60, reading(1.044), t0 + Duration::from_secs(30));
        assert_eq!(fleet.due(t0 + Duration::from_secs(30))[0].reading.gravity, 1.044);
    }

    #[test]
    fn stale_readings_are_dropped_and_never_posted() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -60, reading(1.050), t0);

        let late = t0 + mins(21);
        assert!(fleet.due(late).is_empty(), "stale reading must not be offered");
        assert_eq!(fleet.drop_stale(late), vec!["Yellow Pill 1".to_string()]);
        assert!(fleet.drop_stale(late).is_empty(), "already dropped");

        // A fresh reading works again afterwards.
        fleet.on_heard(A, -60, reading(1.040), late);
        assert_eq!(fleet.due(late).len(), 1);
    }

    #[test]
    fn a_reading_just_inside_the_age_limit_is_still_posted() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -60, reading(1.050), t0);
        assert!(fleet.drop_stale(t0 + mins(20)).is_empty());
        assert_eq!(fleet.due(t0 + mins(20)).len(), 1);
    }

    #[test]
    fn unlisted_pills_are_reported_once_and_never_posted() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        assert_eq!(fleet.on_heard(STRANGER, -60, reading(1.030), t0), Heard::UnlistedNew);
        assert_eq!(fleet.on_heard(STRANGER, -60, reading(1.031), t0), Heard::UnlistedAgain);
        assert!(fleet.due(t0 + mins(60)).is_empty());
    }

    #[test]
    fn listed_pills_report_their_name_to_the_caller() {
        let mut fleet = two_pills();
        assert_eq!(
            fleet.on_heard(B, -60, reading(1.010), Instant::now()),
            Heard::Listed { name: "Red Pill 2", first: true }
        );
    }

    #[test]
    fn only_the_first_sighting_of_each_pill_is_marked_first() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        assert_eq!(fleet.on_heard(A, -60, reading(1.050), t0), Heard::Listed { name: "Yellow Pill 1", first: true });
        assert_eq!(fleet.on_heard(A, -60, reading(1.049), t0), Heard::Listed { name: "Yellow Pill 1", first: false });
        // B has not been heard yet, so its first sighting is still "first".
        assert_eq!(fleet.on_heard(B, -60, reading(1.010), t0), Heard::Listed { name: "Red Pill 2", first: true });
        assert_eq!(fleet.on_heard(B, -60, reading(1.011), t0), Heard::Listed { name: "Red Pill 2", first: false });
        // Posting does not reset it.
        fleet.mark_posted(0, t0);
        assert_eq!(fleet.on_heard(A, -60, reading(1.048), t0), Heard::Listed { name: "Yellow Pill 1", first: false });
    }

    #[test]
    fn empty_fleet_is_discovery_mode() {
        let t0 = Instant::now();
        let mut fleet = Fleet::new(vec![]);
        assert!(fleet.is_empty());
        assert_eq!(fleet.on_heard(A, -60, reading(1.050), t0), Heard::UnlistedNew);
        assert_eq!(fleet.on_heard(B, -60, reading(1.010), t0), Heard::UnlistedNew);
        assert!(fleet.due(t0 + mins(60)).is_empty());
        assert_eq!(fleet.configured().count(), 0);
    }

    fn sighting(addr: Addr, sg: f32, rssi: i8) -> Sighting {
        Sighting { addr, rssi, reading: reading(sg) }
    }

    #[test]
    fn queue_starts_empty_and_take_empties_it() {
        let mut q = SightingQueue::default();
        assert!(q.take().is_empty());
        q.push(sighting(A, 1.050, -60));
        assert_eq!(q.take().len(), 1);
        assert!(q.take().is_empty());
    }

    #[test]
    fn queue_keeps_only_the_newest_sighting_per_address() {
        let mut q = SightingQueue::default();
        q.push(sighting(A, 1.050, -60));
        q.push(sighting(A, 1.049, -55));
        q.push(sighting(A, 1.048, -70));
        let got = q.take();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].reading.gravity, 1.048);
        assert_eq!(got[0].rssi, -70);
    }

    #[test]
    fn queue_holds_separate_pills_in_arrival_order() {
        let mut q = SightingQueue::default();
        q.push(sighting(A, 1.050, -60));
        q.push(sighting(B, 1.010, -65));
        q.push(sighting(A, 1.049, -61)); // update must not reorder or duplicate
        let got = q.take();
        assert_eq!(got.iter().map(|s| s.addr).collect::<Vec<_>>(), vec![A, B]);
        assert_eq!(got[0].reading.gravity, 1.049);
    }

    #[test]
    fn queue_is_capped_but_still_updates_known_pills_when_full() {
        let mut q = SightingQueue::default();
        for i in 0..(SightingQueue::MAX_PILLS as u8 + 10) {
            q.push(sighting([0, 0, 0, 0, 0, i], 1.000, -50));
        }
        // An address already queued can still be refreshed after the cap is reached...
        q.push(sighting([0, 0, 0, 0, 0, 0], 1.099, -40));
        let got = q.take();
        assert_eq!(got.len(), SightingQueue::MAX_PILLS);
        assert_eq!(got[0].reading.gravity, 1.099);
        // ...and the queue accepts new addresses again once drained.
        q.push(sighting([9, 9, 9, 9, 9, 9], 1.000, -50));
        assert_eq!(q.take().len(), 1);
    }

    #[test]
    fn queued_sightings_feed_the_fleet() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        let mut q = SightingQueue::default();
        q.push(sighting(A, 1.050, -60));
        q.push(sighting(STRANGER, 1.030, -80));
        for s in q.take() {
            fleet.on_heard(s.addr, s.rssi, s.reading, t0);
        }
        let due = fleet.due(t0);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].name, "Yellow Pill 1");
    }

    #[test]
    fn configured_lists_pills_in_order() {
        let fleet = two_pills();
        let got: Vec<_> = fleet.configured().map(|(a, n)| (*a, n.to_string())).collect();
        assert_eq!(got, vec![(A, "Yellow Pill 1".to_string()), (B, "Red Pill 2".to_string())]);
    }
    // ---- editing the configured list while running ----

    #[test]
    fn unlisted_pills_are_listed_for_the_settings_page_newest_first() {
        let t0 = Instant::now();
        let mut fleet = Fleet::new(vec![]);
        fleet.on_heard(A, -70, reading(1.010), t0);
        fleet.on_heard(B, -50, reading(1.020), t0 + mins(2));
        let seen = fleet.unlisted(t0 + mins(3));
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].addr, B);
        assert_eq!(seen[0].rssi, -50);
        assert_eq!(seen[0].age, mins(1));
        assert_eq!(seen[1].addr, A);
        assert_eq!(seen[1].age, mins(3));
    }

    #[test]
    fn unlisted_pills_expire_with_stale_readings() {
        let t0 = Instant::now();
        let mut fleet = Fleet::new(vec![]);
        fleet.on_heard(STRANGER, -60, reading(1.0), t0);
        fleet.drop_stale(t0 + MAX_READING_AGE);
        assert_eq!(fleet.unlisted(t0 + MAX_READING_AGE).len(), 1, "exactly at the limit is kept");
        fleet.drop_stale(t0 + MAX_READING_AGE + Duration::from_secs(1));
        assert!(fleet.unlisted(t0 + mins(21)).is_empty());
    }

    #[test]
    fn unlisted_list_is_capped() {
        let t0 = Instant::now();
        let mut fleet = Fleet::new(vec![]);
        for i in 0..40u8 {
            fleet.on_heard([9, 9, 9, 9, 9, i], -60, reading(1.0), t0);
        }
        assert_eq!(fleet.unlisted(t0).len(), SightingQueue::MAX_PILLS);
    }

    #[test]
    fn adding_an_unlisted_pill_posts_its_reading_immediately_without_waiting_for_a_new_advert() {
        let t0 = Instant::now();
        let mut fleet = Fleet::new(vec![]);
        fleet.on_heard(A, -60, reading(1.044), t0);
        assert!(fleet.due(t0).is_empty());

        fleet.set_configured(vec![(A, "New Pill".into())]);
        let due = fleet.due(t0 + mins(1));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].name, "New Pill");
        assert_eq!(due[0].reading.gravity, 1.044);
        assert!(fleet.unlisted(t0 + mins(1)).is_empty(), "no longer shown as unlisted");
    }

    #[test]
    fn keeping_a_pill_with_the_same_name_keeps_its_post_timer() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -60, reading(1.050), t0);
        let idx = fleet.due(t0)[0].idx;
        fleet.mark_posted(idx, t0);

        // The user adds a third Pill; A must not be re-posted early.
        fleet.set_configured(vec![(A, "Yellow Pill 1".into()), (B, "Red Pill 2".into()), (STRANGER, "Third".into())]);
        fleet.on_heard(A, -60, reading(1.049), t0 + mins(5));
        assert!(fleet.due(t0 + mins(5)).is_empty());
        assert_eq!(fleet.due(t0 + mins(15)).len(), 1);
    }

    #[test]
    fn renaming_a_pill_keeps_its_reading_and_posts_at_once_as_a_new_device() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -60, reading(1.050), t0);
        let idx = fleet.due(t0)[0].idx;
        fleet.mark_posted(idx, t0);
        fleet.on_heard(A, -60, reading(1.048), t0 + mins(2));

        fleet.set_configured(vec![(A, "Renamed".into()), (B, "Red Pill 2".into())]);
        let due = fleet.due(t0 + mins(3));
        assert_eq!(due.len(), 1, "inside the old 15 minutes, but this is a new Brewfather device");
        assert_eq!(due[0].name, "Renamed");
        assert_eq!(due[0].reading.gravity, 1.048);
    }

    #[test]
    fn renaming_clears_a_failure_backoff() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -60, reading(1.050), t0);
        let idx = fleet.due(t0)[0].idx;
        fleet.mark_failed(idx, t0);
        assert!(fleet.due(t0 + Duration::from_secs(5)).is_empty());

        fleet.set_configured(vec![(A, "Renamed".into()), (B, "Red Pill 2".into())]);
        assert_eq!(fleet.due(t0 + Duration::from_secs(5)).len(), 1);
    }

    #[test]
    fn removed_pill_is_forgotten_and_shown_as_unlisted_again() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -60, reading(1.050), t0);
        fleet.set_configured(vec![(B, "Red Pill 2".into())]);

        assert!(fleet.due(t0).is_empty(), "a removed Pill is never posted");
        let names: Vec<_> = fleet.configured().map(|(_, n)| n.to_string()).collect();
        assert_eq!(names, ["Red Pill 2"]);
        let unlisted = fleet.unlisted(t0);
        assert_eq!(unlisted.len(), 1);
        assert_eq!(unlisted[0].addr, A);
    }

    #[test]
    fn set_configured_follows_the_new_order() {
        let mut fleet = two_pills();
        fleet.set_configured(vec![(B, "Red Pill 2".into()), (A, "Yellow Pill 1".into())]);
        let order: Vec<_> = fleet.configured().map(|(a, _)| *a).collect();
        assert_eq!(order, [B, A]);
    }

    #[test]
    fn status_keeps_the_last_reading_after_it_has_been_posted() {
        let t0 = Instant::now();
        let mut fleet = two_pills();
        fleet.on_heard(A, -64, reading(1.050), t0);
        let idx = fleet.due(t0)[0].idx;
        fleet.mark_posted(idx, t0);

        let status = fleet.status(t0 + mins(4));
        let a = status.iter().find(|s| s.addr == A).unwrap();
        let (r, rssi, age) = a.last.unwrap();
        assert_eq!((r.gravity, rssi, age), (1.050, -64, mins(4)));
        let b = status.iter().find(|s| s.addr == B).unwrap();
        assert!(b.last.is_none());
    }
}

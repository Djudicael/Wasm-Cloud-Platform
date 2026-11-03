use super::*;
fn healthy() -> Sample {
    Sample {
        available_pages: 200_000,
        some: 0.,
        full: 0.,
        oom: 0,
        high: 0,
    }
}
#[test]
fn isolated_reclaim_with_available_memory_never_rejects() {
    let now = Instant::now();
    let mut state = State {
        last_reclaim: Some(now),
        ..State::default()
    };
    for _ in 0..60 {
        assert_eq!(
            state.sample(healthy(), &MonitorConfig::default(), now),
            None
        );
    }
    assert_eq!(state.level, 0);
}
#[test]
fn sustained_stalls_need_correlation_and_hysteretic_healthy_recovery() {
    let now = Instant::now();
    let mut state = State::default();
    let stalled = Sample {
        full: 2.,
        ..healthy()
    };
    for _ in 0..10 {
        assert_eq!(state.sample(stalled, &MonitorConfig::default(), now), None);
    }
    state.last_reclaim = Some(now);
    assert_eq!(state.sample(stalled, &MonitorConfig::default(), now), None);
    assert_eq!(state.sample(stalled, &MonitorConfig::default(), now), None);
    assert_eq!(
        state.sample(stalled, &MonitorConfig::default(), now),
        Some(1)
    );
    for _ in 0..4 {
        assert_eq!(
            state.sample(healthy(), &MonitorConfig::default(), now),
            None
        );
    }
    assert_eq!(
        state.sample(healthy(), &MonitorConfig::default(), now),
        Some(0)
    );
}
#[test]
fn critical_availability_and_new_oom_do_not_wait_for_medium_window() {
    let now = Instant::now();
    let mut state = State::default();
    state.sample(healthy(), &MonitorConfig::default(), now);
    assert_eq!(
        state.sample(
            Sample {
                oom: 1,
                ..healthy()
            },
            &MonitorConfig::default(),
            now
        ),
        Some(2)
    );
    let mut other = State::default();
    assert_eq!(
        other.sample(
            Sample {
                available_pages: 1,
                ..healthy()
            },
            &MonitorConfig::default(),
            now
        ),
        Some(2)
    );
}
#[test]
fn interrupted_low_memory_and_healthy_windows_do_not_accumulate() {
    let now = Instant::now();
    let mut state = State::default();
    let low = Sample {
        available_pages: 50_000,
        ..healthy()
    };
    state.sample(low, &MonitorConfig::default(), now);
    state.sample(healthy(), &MonitorConfig::default(), now);
    state.sample(low, &MonitorConfig::default(), now);
    assert_eq!(state.sample(low, &MonitorConfig::default(), now), None);
    assert_eq!(state.sample(low, &MonitorConfig::default(), now), Some(1));
    state.sample(healthy(), &MonitorConfig::default(), now);
    state.sample(
        Sample {
            some: 7.,
            ..healthy()
        },
        &MonitorConfig::default(),
        now,
    );
    for _ in 0..4 {
        assert_eq!(
            state.sample(healthy(), &MonitorConfig::default(), now),
            None
        );
    }
    assert_eq!(
        state.sample(healthy(), &MonitorConfig::default(), now),
        Some(0)
    );
}

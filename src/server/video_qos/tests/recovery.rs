use super::*;

#[test]
fn automatic_feedback_never_caps_below_thirty_fps_when_the_user_allows_it() {
    let mut qos = super::smoke::session(60, Quality::Balanced);
    for _ in 0..30 {
        qos.user_network_delay(1, 10);
    }
    qos.user_auto_adjust_fps(1, 10);
    assert_eq!(qos.fps(), 30);
}

#[test]
fn a_legacy_viewers_automatic_measurements_cannot_cap_below_thirty_fps() {
    let mut qos = super::smoke::session(60, Quality::Balanced);
    for _ in 0..30 {
        qos.user_network_delay(1, 10);
    }
    assert_eq!(qos.fps(), 60);

    // A viewer without auto_adjust_fps reports its automatic measurement here:
    // the first value is its setting, later ones must not ratchet the stream
    // below interactive rates.
    qos.user_custom_fps(1, 11);
    assert_eq!(qos.users[&1].custom_fps, Some(30));
    assert_eq!(qos.fps(), 30);

    qos.user_custom_fps(1, 42);
    assert_eq!(qos.users[&1].custom_fps, Some(42));
    assert_eq!(qos.fps(), 42);

    qos.user_custom_fps(1, 6);
    assert_eq!(qos.users[&1].custom_fps, Some(30));
    assert_eq!(qos.fps(), 30);

    // A first value stays as it is, however low.
    qos.users.insert(2, UserData::default());
    qos.user_custom_fps(2, 12);
    assert_eq!(qos.users[&2].custom_fps, Some(12));
}

#[test]
fn automatic_cap_drops_immediately_and_recovers_in_steps() {
    let mut qos = super::smoke::session(60, Quality::Balanced);
    for _ in 0..30 {
        qos.user_network_delay(1, 10);
    }
    assert_eq!(qos.fps(), 60);

    qos.user_auto_adjust_fps(1, 10);
    assert_eq!(qos.fps(), 30);
    assert_eq!(qos.users[&1].custom_fps, Some(60));

    qos.user_auto_adjust_fps(1, 43);
    assert_eq!(qos.fps(), 35);
    assert_eq!(qos.users[&1].custom_fps, Some(60));
}

#[test]
fn queue_recovery_waits_for_low_delay_and_raises_fps_gradually() {
    let mut qos = super::smoke::session(60, Quality::Balanced);
    for _ in 0..30 {
        qos.user_network_delay(1, 50);
    }
    assert_eq!(qos.fps(), 60);

    for _ in 0..2 {
        qos.user_network_delay(1, 1200);
    }
    let reduced = qos.fps();
    assert!(reduced < 30);
    for _ in 0..3 {
        qos.user_network_delay(1, 125);
        assert_eq!(qos.fps(), reduced, "queue still adds 75 ms");
    }

    let mut previous = reduced;
    for _ in 0..16 {
        qos.user_network_delay(1, 50);
        let fps = qos.fps();
        assert!(
            (previous..=previous + 5).contains(&fps),
            "{previous} → {fps}"
        );
        previous = fps;
    }
    assert_eq!(qos.fps(), 60);
}

#[test]
fn ordinary_congestion_waits_between_bounded_cuts() {
    for delay in [400, 800] {
        let mut qos = stable_qos();
        for _ in 0..2 {
            qos.user_network_delay(1, delay);
            assert_eq!(qos.fps(), FPS);
        }
        qos.user_network_delay(1, delay);
        let first_cut = qos.fps();
        assert!((24..FPS).contains(&first_cut), "delay={delay}: {first_cut}");
        for _ in 0..2 {
            qos.user_network_delay(1, delay);
            assert_eq!(qos.fps(), first_cut, "wait for new evidence after a cut");
        }
        qos.user_network_delay(1, delay);
        assert!(qos.fps() < first_cut);
        assert!(qos.fps() >= first_cut - first_cut / 5);
    }
}

#[test]
fn automatic_floor_preserves_lower_custom_limits() {
    for limit in [1, 3, 5, 30, 60, 120] {
        for abr in [false, true] {
            for timeout in [false, true] {
                let mut qos = super::smoke::session(limit, Quality::Balanced);
                qos.abr_config = abr;
                qos.new_display("test".to_owned());
                qos.set_support_changing_quality("test", true);
                for _ in 0..90 {
                    qos.user_network_delay(1, 10);
                }
                assert_eq!(qos.fps(), limit);
                qos.ratio = BR_MIN_HIGH_RESOLUTION;
                qos.store_bitrate(600);
                let floor = 5.min(limit);
                if timeout {
                    for elapsed in [2001, 3001, 4001, 5001, 10_000, 30_000] {
                        qos.user_delay_response_elapsed(1, elapsed);
                        assert!(
                            (floor..=limit).contains(&qos.fps()),
                            "timeout={elapsed}, limit={limit}, ABR={abr}"
                        );
                    }
                } else {
                    for _ in 0..8 {
                        qos.user_network_delay(1, 1500);
                        assert!(
                            (floor..=limit).contains(&qos.fps()),
                            "limit={limit}, ABR={abr}"
                        );
                    }
                }
                assert_eq!(
                    qos.fps(),
                    floor,
                    "timeout={timeout}, limit={limit}, ABR={abr}"
                );
                if timeout {
                    qos.user_network_delay(1, 30_100);
                    assert_eq!(qos.fps(), floor, "a late reply must preserve the floor");
                }
                for _ in 0..(limit - floor).div_ceil(5) + 2 {
                    qos.user_network_delay(1, 10);
                }
                assert_eq!(qos.fps(), limit, "recover: timeout={timeout}, ABR={abr}");
            }
        }
    }
}

#[test]
fn jitter_during_recovery_does_not_discard_the_restore_target() {
    let mut qos = stable_qos();
    for _ in 0..3 {
        qos.user_network_delay(1, 1200);
    }
    for delay in [10, 350, 10] {
        qos.user_network_delay(1, delay);
    }
    for _ in 0..5 {
        qos.user_network_delay(1, 10);
    }
    assert_eq!(qos.fps(), FPS);
}

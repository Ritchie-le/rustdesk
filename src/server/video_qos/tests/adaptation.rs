use super::*;

const DISPLAY: &str = "adaptation";

fn session(abr: bool) -> VideoQoS {
    let mut qos = super::smoke::session(FPS, Quality::Balanced);
    qos.abr_config = abr;
    qos.new_display(DISPLAY.to_owned());
    qos.set_support_changing_quality(DISPLAY, true);
    sync_bitrate(&mut qos);
    qos
}

fn sync_bitrate(qos: &mut VideoQoS) {
    let bitrate = (6000.0 * qos.ratio()) as u32;
    qos.store_bitrate(bitrate);
}

fn second(qos: &mut VideoQoS, delay: u32, dynamic: bool) {
    let encoded = if dynamic { qos.fps() as usize } else { 0 };
    qos.advance_ms(1000);
    sync_bitrate(qos);
    qos.user_network_delay(1, delay);
    sync_bitrate(qos);
    qos.update_display_data(DISPLAY, encoded);
    sync_bitrate(qos);
}

#[test]
fn user_quality_is_a_ceiling_not_a_starting_point() {
    let mut qos = session(true);
    // The viewer's stored "custom 1000" setting arrives as quality << 8.
    qos.user_image_quality(1, 1000 << 8);
    assert_eq!(qos.latest_quality(), Quality::Custom(20.0));
    assert_eq!(
        qos.ratio(),
        BR_BALANCED,
        "a high request must not start the encoder at its ceiling"
    );
    for _ in 0..30 {
        second(&mut qos, 10, true);
    }
    let probed = qos.ratio();
    assert!(
        probed > BR_BALANCED,
        "a healthy link is probed upward, got {probed}"
    );
    assert!(probed < 20.0, "probing stays far below the ceiling");
    // A lower request applies at once: quality 25 => ratio 0.5.
    qos.user_image_quality(1, 25 << 8);
    assert_eq!(qos.ratio(), 0.5);
}

#[test]
fn a_new_session_starts_from_the_conservative_ratio() {
    let mut qos = session(true);
    qos.user_image_quality(1, 1000 << 8);
    for _ in 0..30 {
        second(&mut qos, 10, true);
    }
    assert!(
        qos.ratio() > BR_BALANCED,
        "the previous session probed the link upward: {}",
        qos.ratio()
    );
    // A departing viewer's entry can outlive its connection; the next session
    // must not inherit the probed ratio of the previous one.
    qos.on_connection_open(2);
    assert_eq!(qos.ratio(), BR_BALANCED);
}

#[test]
fn static_to_dynamic_ratio_recovery() {
    println!("Static recovery: 90 s healthy video, 12 s confirmed 800 ms delay, 60 s healthy static screen, then 90 s video. Bitrate is modeled as ratio * 6000 kbps.");
    println!("| restart profile | ratio after cut | ratio after static | time to 90% | time to 95% | final ratio | final FPS | modeled bitrate |");
    println!("|---|---:|---:|---|---|---:|---:|---:|");
    for (profile, restart_delay, growing_queue) in [
        ("healthy 10 ms", 10, false),
        ("stable path 800 ms", 800, false),
        ("growing queue 800 + 10 ms/s", 800, true),
    ] {
        let mut qos = session(true);
        for _ in 0..90 {
            second(&mut qos, 10, true);
        }
        let target = qos.latest_quality().ratio();
        assert_eq!(qos.ratio(), target);
        for _ in 0..12 {
            second(&mut qos, 800, true);
        }
        let after_cut = qos.ratio();
        assert!(
            after_cut < target * 0.5,
            "fixture must confirm congestion and cut bitrate"
        );
        for _ in 0..60 {
            second(&mut qos, 10, false);
        }
        let after_static = qos.ratio();
        let mut t90 = (after_static >= target * 0.90).then_some(0);
        let mut t95 = (after_static >= target * 0.95).then_some(0);
        for s in 1..=90 {
            let delay = restart_delay + if growing_queue { s * 10 } else { 0 };
            second(&mut qos, delay, true);
            let ratio = qos.ratio();
            if ratio >= target * 0.90 {
                t90.get_or_insert(s);
            }
            if ratio >= target * 0.95 {
                t95.get_or_insert(s);
            }
            if growing_queue {
                assert!(
                    ratio <= after_static * 1.02,
                    "activity must not restore quality into congestion"
                );
            }
        }
        let seconds = |time: Option<u32>| {
            time.map(|s| format!("{s} s"))
                .unwrap_or_else(|| "never".to_owned())
        };
        let ratio = qos.ratio();
        println!("| {profile} | {after_cut:.3} | {after_static:.3} | {} | {} | {ratio:.3} | {} | {} kbps |",
            seconds(t90), seconds(t95), qos.fps(), qos.bitrate());
        if !growing_queue {
            assert!(
                t95.is_some(),
                "{profile}: video did not regain 95% quality within 90 s"
            );
            assert_eq!(qos.fps(), FPS);
        }
    }
}

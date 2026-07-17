/*
File: crates/ms-gifs/tests/assets_decode.rs

Purpose:
Validates every shipped WebP hint at CI time, including streaming playback,
frame layout, timing, looping, visible pixels, and stable-name uniqueness.
*/

use std::collections::HashSet;

#[test]
fn every_embedded_hint_streams_a_valid_looping_animation() {
    let mut names = HashSet::new();

    for hint in ms_gifs::all() {
        assert!(names.insert(hint.name()), "duplicate hint name: {}", hint.name());
        let mut player = ms_gifs::Player::open(*hint)
            .unwrap_or_else(|error| panic!("failed to open {}: {error}", hint.name()));
        assert_eq!(player.hint(), *hint);
        assert!(player.width() > 0, "{} has zero width", hint.name());
        assert!(player.height() > 0, "{} has zero height", hint.name());
        assert!(player.frame_count() > 1, "{} is not animated", hint.name());

        let mut frame = vec![0; player.frame_buffer_len()];
        let mut first_frame = Vec::new();
        let mut previous_frame = Vec::new();
        let mut has_nonzero_pixel = false;
        let mut has_changed_frame = false;

        for index in 0..player.frame_count() {
            let delay = player.next_frame(&mut frame).unwrap_or_else(|error| {
                panic!("failed to decode {} frame {index}: {error}", hint.name())
            });
            assert!(!delay.is_zero(), "{} frame {index} has zero delay", hint.name());
            has_nonzero_pixel |= frame
                .chunks_exact(4)
                .any(|pixel| pixel.iter().any(|channel| *channel != 0));
            if index == 0 {
                first_frame.clone_from(&frame);
            } else {
                has_changed_frame |= frame != previous_frame;
            }
            previous_frame.clone_from(&frame);
        }

        assert!(has_nonzero_pixel, "{} decoded to fully transparent black", hint.name());
        assert!(has_changed_frame, "{} has only byte-identical frames", hint.name());

        player
            .next_frame(&mut frame)
            .unwrap_or_else(|error| panic!("failed to loop {}: {error}", hint.name()));
        assert_eq!(frame, first_frame, "{} loop did not restart at frame zero", hint.name());

        println!(
            "{}: {} frames, {}x{}",
            hint.name(),
            player.frame_count(),
            player.width(),
            player.height()
        );
    }
}

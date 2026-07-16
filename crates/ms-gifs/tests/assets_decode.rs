/*
File: crates/ms-gifs/tests/assets_decode.rs

Purpose:
Validates every shipped WebP hint at CI time, including animation structure,
frame layout, timing, and stable-name uniqueness.
*/

use std::collections::HashSet;

#[test]
fn every_embedded_hint_decodes_to_valid_animation() {
    let mut names = HashSet::new();

    for hint in ms_gifs::all() {
        assert!(names.insert(hint.name()), "duplicate hint name: {}", hint.name());
        let animation = ms_gifs::decode(*hint)
            .unwrap_or_else(|error| panic!("failed to decode {}: {error}", hint.name()));
        assert!(animation.width > 0, "{} has zero width", hint.name());
        assert!(animation.height > 0, "{} has zero height", hint.name());
        assert!(animation.frames.len() > 1, "{} is not animated", hint.name());

        let expected_len = usize::try_from(animation.width)
            .unwrap()
            .checked_mul(usize::try_from(animation.height).unwrap())
            .and_then(|pixels| pixels.checked_mul(4))
            .unwrap();
        for (index, frame) in animation.frames.iter().enumerate() {
            assert_eq!(frame.rgba.len(), expected_len, "{} frame {index}", hint.name());
            assert!(!frame.delay.is_zero(), "{} frame {index} has zero delay", hint.name());
        }
        assert!(
            animation
                .frames
                .iter()
                .flat_map(|frame| frame.rgba.chunks_exact(4))
                .any(|pixel| pixel.iter().any(|channel| *channel != 0)),
            "{} decoded to fully transparent black",
            hint.name()
        );
        assert!(
            animation
                .frames
                .windows(2)
                .any(|frames| frames[0].rgba != frames[1].rgba),
            "{} has only byte-identical frames",
            hint.name()
        );

        println!(
            "{}: {} frames, {}x{}",
            hint.name(),
            animation.frames.len(),
            animation.width,
            animation.height
        );
    }
}

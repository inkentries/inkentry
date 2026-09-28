use inkentry_core::embeddings::{blob_to_vec, vec_to_blob};
use proptest::prelude::*;

proptest! {
    #[test]
    fn vec_blob_roundtrip(
        values in prop::collection::vec(-1e6f32..1e6f32, 0..=512)
    ) {
        let blob = vec_to_blob(&values);
        let recovered = blob_to_vec(&blob);
        prop_assert_eq!(values.len(), recovered.len());
        for (a, b) in values.iter().zip(recovered.iter()) {
            prop_assert!(
                (a - b).abs() < 1e-6,
                "value {} recovered as {}",
                a,
                b
            );
        }
    }

    #[test]
    fn blob_length_is_four_bytes_per_float(
        values in prop::collection::vec(-1.0f32..1.0f32, 0..=256)
    ) {
        let blob = vec_to_blob(&values);
        prop_assert_eq!(blob.len(), values.len() * 4);
    }
}

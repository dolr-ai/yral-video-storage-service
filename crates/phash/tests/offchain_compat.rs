mod offchain_compat {
    use phash::{PHashError, PHasher, EXPECTED_BINARY_HASH_LEN, HASH_KIND, HASH_VERSION};
    use serde::Serialize;

    const FIXTURE_VIDEO: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/test-raw-video.mp4"
    );
    const EXPECTED_HASH: &str = include_str!("fixtures/test_raw_video.offchain_binary_10x8_v1.txt");

    #[test]
    fn offchain_binary_hash_contract_matches_fixture() -> Result<(), PHashError> {
        let hasher = PHasher::new();
        let actual = hasher.hash_video(FIXTURE_VIDEO)?;
        let expected = EXPECTED_HASH.trim();

        assert_eq!(actual.len(), EXPECTED_BINARY_HASH_LEN);
        assert!(actual.bytes().all(|b| b == b'0' || b == b'1'));
        assert_eq!(actual, expected);
        Ok(())
    }

    #[test]
    fn metadata_is_extracted_with_hash() -> Result<(), PHashError> {
        let hasher = PHasher::new();
        let result = hasher.hash_video_with_metadata(FIXTURE_VIDEO)?;

        assert_eq!(result.hash.len(), EXPECTED_BINARY_HASH_LEN);
        assert_eq!(result.hash_kind, HASH_KIND);
        assert_eq!(result.hash_version, HASH_VERSION);
        assert!(result.metadata.duration_seconds > 0.0);
        assert!(result.metadata.frame_count > 0);
        assert!(result.metadata.width > 0);
        assert!(result.metadata.height > 0);
        Ok(())
    }

    #[test]
    fn hash_result_is_serializable_for_storage_and_feed_payloads() -> Result<(), PHashError> {
        fn assert_serialize<T: Serialize>(value: &T) -> Result<(), serde_json::Error> {
            serde_json::to_value(value).map(|_| ())
        }

        let hasher = PHasher::new();
        let result = hasher.hash_video_with_metadata(FIXTURE_VIDEO)?;

        assert_serialize(&result)?;
        assert_serialize(&result.metadata)?;
        Ok(())
    }
}

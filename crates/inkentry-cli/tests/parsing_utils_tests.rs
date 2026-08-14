// Consolidated parsing/plumbing-utility test binary: groups the previously separate chunk/context/embed/hash/knn/parse test files into one integration test crate to cut per-binary link overhead.

mod plumbing_helpers;

#[path = "parsing_utils_tests/cat_chunks.rs"]
mod cat_chunks;
#[path = "parsing_utils_tests/context.rs"]
mod context;
#[path = "parsing_utils_tests/embed.rs"]
mod embed;
#[path = "parsing_utils_tests/hash_file.rs"]
mod hash_file;
#[path = "parsing_utils_tests/index_embed_tier_routing.rs"]
mod index_embed_tier_routing;
#[path = "parsing_utils_tests/knn.rs"]
mod knn;
#[path = "parsing_utils_tests/ls_files.rs"]
mod ls_files;
#[path = "parsing_utils_tests/parse_file.rs"]
mod parse_file;

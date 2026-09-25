// Code outside every matched node used to be dropped whenever a file had at
// least one match: module-level statements, and the body of a container too
// large to keep whole, whose own chunk is suppressed in favour of its members.

use inkentry_core::indexer::{Chunk, SourceParser};

fn covers(chunks: &[Chunk], needle: &str) -> bool {
    chunks.iter().any(|c| c.content.contains(needle))
}

#[test]
fn module_level_code_beside_a_matched_node_is_still_indexed() {
    let src = "export interface Props { value: string }\n\n\
               export const PUBLIC_PATHS = ['/login', '/signup', '/forgot-password']\n\n\
               describe('picker', () => { it('renders the selected value', () => {}) })\n";
    let chunks = SourceParser::parse(src, "picker.test.ts", "typescript").unwrap();
    assert!(covers(&chunks, "PUBLIC_PATHS"), "{chunks:#?}");
    assert!(covers(&chunks, "renders the selected value"), "{chunks:#?}");
}

#[test]
fn the_body_of_an_oversized_ruby_class_keeps_its_associations() {
    let methods: String = (0..40)
        .map(|i| {
            format!("  def method_{i}\n    compute_amount_{i}(fees, taxes, credits)\n  end\n\n")
        })
        .collect();
    let src = format!(
        "class Invoice < ApplicationRecord\n  belongs_to :customer\n  has_many :fees\n  \
         has_many :credit_notes\n  validates :currency, presence: true\n\n{methods}end\n"
    );
    let chunks = SourceParser::parse(&src, "invoice.rb", "ruby").unwrap();
    assert!(
        chunks.iter().any(|c| c.name.as_deref() == Some("method_0")),
        "the members are still chunked: {chunks:#?}"
    );
    for line in [
        "belongs_to :customer",
        "has_many :credit_notes",
        "validates :currency",
    ] {
        assert!(covers(&chunks, line), "{line} must be indexed: {chunks:#?}");
    }
}

#[test]
fn a_gap_of_closing_punctuation_is_not_a_chunk() {
    let src = "fn a() {\n    let x = 1;\n}\n}\n\nfn b() {\n    let y = 2;\n}\n";
    let chunks = SourceParser::parse(src, "lib.rs", "rust").unwrap();
    assert_eq!(chunks.len(), 2, "{chunks:#?}");
}

#[test]
fn a_doc_comment_is_not_indexed_again_as_a_gap() {
    let src = "/// Adds two numbers together and returns their sum.\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
    let chunks = SourceParser::parse(src, "lib.rs", "rust").unwrap();
    assert_eq!(chunks.len(), 1, "{chunks:#?}");
    assert!(
        chunks[0]
            .docstring
            .as_deref()
            .is_some_and(|d| d.contains("Adds two numbers"))
    );
}

#[test]
fn gap_windows_carry_their_true_line_span_and_chunks_stay_in_source_order() {
    let src = "fn a() {}\n\nconst LIMIT: usize = 4096; // a module-level constant\nstatic NAMES: [&str; 2] = [\"alpha\", \"beta\"];\n\nfn b() {}\n";
    let chunks = SourceParser::parse(src, "lib.rs", "rust").unwrap();
    let gap = chunks
        .iter()
        .find(|c| c.content.contains("static NAMES"))
        .expect("the static is indexed");
    assert_eq!((gap.start_line, gap.end_line), (4, 4), "{chunks:#?}");
    let starts: Vec<usize> = chunks.iter().map(|c| c.start_line).collect();
    let mut sorted = starts.clone();
    sorted.sort();
    assert_eq!(starts, sorted);
}

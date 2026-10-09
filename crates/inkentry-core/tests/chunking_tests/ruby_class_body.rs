// A Ruby class too large to keep whole has its class-level declarations split
// into runs of one kind, each named for what it declares. Windowed together,
// a model's associations, validations and scopes embed as the model in general
// and match no particular question about it.

use inkentry_core::indexer::{Chunk, SourceParser};

fn methods(indent: &str) -> String {
    (0..40)
        .map(|i| {
            format!(
                "{indent}def method_{i}\n{indent}  compute_amount_{i}(fees, taxes, credits)\n{indent}end\n\n"
            )
        })
        .collect()
}

fn model(body: &str) -> String {
    format!(
        "class Invoice < ApplicationRecord\n{body}\n{}end\n",
        methods("  ")
    )
}

fn parse(src: &str) -> Vec<Chunk> {
    SourceParser::parse(src, "app/models/invoice.rb", "ruby").unwrap()
}

fn named<'a>(chunks: &'a [Chunk], name: &str) -> Vec<&'a Chunk> {
    chunks
        .iter()
        .filter(|c| c.name.as_deref() == Some(name))
        .collect()
}

fn one_named<'a>(chunks: &'a [Chunk], name: &str) -> &'a Chunk {
    match named(chunks, name).as_slice() {
        [only] => only,
        other => panic!("expected one chunk named {name:?}, got {other:#?}\nall: {chunks:#?}"),
    }
}

fn holding<'a>(chunks: &'a [Chunk], needle: &str) -> &'a Chunk {
    chunks
        .iter()
        .find(|c| c.kind.to_string() == "verbatim" && c.content.contains(needle))
        .unwrap_or_else(|| panic!("no window holds {needle:?}: {chunks:#?}"))
}

#[test]
fn consecutive_associations_are_one_chunk_named_for_them() {
    let src = model(
        "  belongs_to :customer\n  has_many :fees\n  has_one :tax_summary\n  \
         has_and_belongs_to_many :tags\n",
    );
    let chunks = parse(&src);

    let associations = one_named(&chunks, "Invoice associations");
    assert_eq!((associations.start_line, associations.end_line), (2, 5));
    assert_eq!(associations.parent_scope, None);
}

#[test]
fn validations_are_one_chunk() {
    let src = model(
        "  validates :currency, presence: true\n  validate :validate_dates\n  \
         validates_with TotalValidator\n  validates_presence_of :number\n",
    );
    let chunks = parse(&src);
    let validations = one_named(&chunks, "Invoice validations");
    assert_eq!((validations.start_line, validations.end_line), (2, 5));
}

#[test]
fn scopes_keep_a_multi_line_lambda_whole() {
    let src = model(
        "  default_scope -> { kept }\n  scope :ready, lambda {\n    \
         where(status: :draft)\n      .where(ready: true)\n  }\n",
    );
    let chunks = parse(&src);
    let scopes = one_named(&chunks, "Invoice scopes");
    assert_eq!((scopes.start_line, scopes.end_line), (2, 6));
}

#[test]
fn callbacks_are_one_chunk() {
    let src =
        model("  before_save :ensure_number\n  after_commit :notify\n  around_update :lock\n");
    let chunks = parse(&src);
    let callbacks = one_named(&chunks, "Invoice callbacks");
    assert_eq!((callbacks.start_line, callbacks.end_line), (2, 4));
}

#[test]
fn interleaved_attribute_and_enum_lines_are_one_attributes_chunk() {
    let src = model(
        "  attribute :status, :string\n  enum :status, STATUSES\n  monetize :total_cents\n  \
         store_accessor :settings, :theme\n  encrypts :token\n  serialize :data\n  \
         normalizes :email, with: ->(e) { e.strip }\n  alias_attribute :amount, :total_cents\n",
    );
    let chunks = parse(&src);
    let attributes = one_named(&chunks, "Invoice attributes");
    assert_eq!((attributes.start_line, attributes.end_line), (2, 9));
}

#[test]
fn delegations_are_one_chunk() {
    let src = model(
        "  delegate :currency, to: :customer\n  delegate :name, to: :organization, prefix: true\n",
    );
    let chunks = parse(&src);
    let delegations = one_named(&chunks, "Invoice delegations");
    assert_eq!((delegations.start_line, delegations.end_line), (2, 3));
}

#[test]
fn a_multi_line_constant_is_one_constants_chunk() {
    let src = model("  STATUSES = %w[\n    draft\n    finalized\n    voided\n  ].freeze\n");
    let chunks = parse(&src);
    let constants = one_named(&chunks, "Invoice constants");
    assert_eq!((constants.start_line, constants.end_line), (2, 6));
}

#[test]
fn an_unrecognised_statement_extends_the_run_before_it() {
    let src = model(
        "  include Discard::Model\n  include PaperTrailTraceable\n\n  \
         has_many :fees\n  sequenced scope: ->(invoice) { invoice.organization.invoices }\n",
    );
    let chunks = parse(&src);

    let head = one_named(&chunks, "Invoice");
    assert!(head.content.contains("class Invoice"), "{head:#?}");
    assert!(
        head.content.contains("include PaperTrailTraceable"),
        "{head:#?}"
    );
    let associations = one_named(&chunks, "Invoice associations");
    assert!(
        associations.content.contains("sequenced scope:"),
        "{associations:#?}"
    );
}

#[test]
fn a_comment_above_a_statement_goes_with_it() {
    let src = model(
        "  has_many :fees\n\n  # Only invoices that can still be edited.\n  \
         scope :draft, -> { where(status: :draft) }\n",
    );
    let chunks = parse(&src);

    let scopes = one_named(&chunks, "Invoice scopes");
    assert!(
        scopes.content.starts_with("  # Only invoices"),
        "{scopes:#?}"
    );
    assert!(
        !one_named(&chunks, "Invoice associations")
            .content
            .contains("# Only")
    );
}

#[test]
fn a_family_that_recurs_after_another_is_a_second_chunk_in_source_order() {
    let src =
        model("  has_many :fees\n  validates :number, presence: true\n  belongs_to :customer\n");
    let chunks = parse(&src);

    let associations = named(&chunks, "Invoice associations");
    assert_eq!(associations.len(), 2, "{chunks:#?}");
    assert!(associations[0].content.contains("has_many :fees"));
    assert!(associations[1].content.contains("belongs_to :customer"));
    let validations = one_named(&chunks, "Invoice validations");
    assert!(associations[0].end_line < validations.start_line);
    assert!(validations.end_line < associations[1].start_line);
}

#[test]
fn runs_between_methods_are_named_and_a_lone_private_is_not_a_chunk() {
    let src = format!(
        "class Invoice < ApplicationRecord\n  has_many :fees\n\n{}  \
         private\n\n{}  delegate :currency, to: :customer\n  delegate :name, to: :organization\nend\n",
        methods("  "),
        methods("  ")
    );
    let chunks = parse(&src);

    let delegations = one_named(&chunks, "Invoice delegations");
    assert!(
        delegations.content.contains("delegate :name"),
        "{delegations:#?}"
    );
    assert!(
        !chunks.iter().any(|c| c.content.trim() == "private"),
        "{chunks:#?}"
    );
}

#[test]
fn a_run_over_the_token_cap_is_windowed_under_one_name() {
    let associations: String = (0..200)
        .map(|i| format!("  has_many :related_records_{i}, class_name: \"RelatedRecord{i}\"\n"))
        .collect();
    let chunks = parse(&model(&associations));

    let windows = named(&chunks, "Invoice associations");
    assert!(windows.len() > 1, "{chunks:#?}");
    assert!(windows.iter().all(|w| w.kind.to_string() == "verbatim"));
}

#[test]
fn a_class_in_modules_takes_the_innermost_class_name_and_its_parent_scope() {
    let src = format!(
        "module Billing\n  module Ledger\n    class Entry < ApplicationRecord\n      \
         belongs_to :invoice\n      belongs_to :account\n\n{}    end\n  end\nend\n",
        methods("      ")
    );
    let chunks = SourceParser::parse(&src, "billing/ledger/entry.rb", "ruby").unwrap();

    let associations = one_named(&chunks, "Entry associations");
    let member = named(&chunks, "method_0")[0];
    let head = one_named(&chunks, "Entry");
    assert_eq!(associations.parent_scope, head.parent_scope);
    assert_eq!(member.parent_scope.as_deref(), Some("class Entry"));
}

#[test]
fn a_class_small_enough_to_keep_whole_is_one_chunk() {
    let src = "class Tag < ApplicationRecord\n  belongs_to :organization\n  \
               validates :name, presence: true\n\n  def label\n    name.titleize\n  end\nend\n";
    let chunks = SourceParser::parse(src, "app/models/tag.rb", "ruby").unwrap();

    assert!(
        chunks.iter().all(|c| c.kind.to_string() != "verbatim"),
        "{chunks:#?}"
    );
    assert!(
        !chunks
            .iter()
            .any(|c| c.name.as_deref().is_some_and(|n| n.starts_with("Tag "))),
        "{chunks:#?}"
    );
}

#[test]
fn a_python_class_body_keeps_its_declaration_window() {
    let methods: String = (0..40)
        .map(|i| {
            format!("    def method_{i}(self):\n        return compute_amount_{i}(self.fees)\n\n")
        })
        .collect();
    let src = format!(
        "class Invoice(models.Model):\n    belongs_to = models.ForeignKey(Customer)\n    \
         validates = models.BooleanField()\n\n{methods}"
    );
    let chunks = SourceParser::parse(&src, "billing/models.py", "python").unwrap();

    let head = holding(&chunks, "belongs_to = models.ForeignKey");
    assert_eq!(head.name.as_deref(), Some("Invoice"), "{head:#?}");
    assert!(head.content.contains("validates = models.BooleanField"));
}

#[test]
fn every_class_level_line_lands_in_a_chunk() {
    let src = format!(
        "# An invoice.\nclass Invoice < ApplicationRecord\n  include Discard::Model\n\n  \
         STATUSES = %w[draft finalized].freeze\n\n  # Who it is billed to.\n  \
         belongs_to :customer\n  has_many :fees\n\n  before_save :ensure_number\n  \
         scope :draft, -> {{ where(status: :draft) }}\n\n{}  \
         private\n\n  attr_reader :billing_period\n  # trailing note about the reader\nend\n",
        methods("  ")
    );
    let chunks = parse(&src);

    let method_lines: std::collections::HashSet<usize> = chunks
        .iter()
        .filter(|c| c.kind.to_string() == "method")
        .flat_map(|c| c.start_line..=c.end_line)
        .collect();
    for (i, line) in src.lines().enumerate() {
        let n = i + 1;
        let t = line.trim();
        if t.is_empty() || t == "end" || t == "private" || method_lines.contains(&n) {
            continue;
        }
        assert!(
            chunks.iter().any(|c| c.start_line <= n && n <= c.end_line),
            "line {n} {line:?} is in no chunk: {chunks:#?}"
        );
    }
}

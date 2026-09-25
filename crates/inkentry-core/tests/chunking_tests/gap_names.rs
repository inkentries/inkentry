// The window holding the declaration of a container too large to keep whole
// is named after it. Without a name it lost the heaviest full-text signal: a
// Rails model's associations and validations were findable only by their
// body text.

use inkentry_core::indexer::{Chunk, SourceParser};

fn window_with<'a>(chunks: &'a [Chunk], needle: &str) -> &'a Chunk {
    chunks
        .iter()
        .find(|c| c.kind.to_string() == "verbatim" && c.content.contains(needle))
        .unwrap_or_else(|| panic!("no window holds {needle:?}: {chunks:#?}"))
}

// Enough members to push the enclosing container past the chunk token cap,
// which is what suppresses its own chunk and leaves its body to gap windows.
fn ruby_methods(indent: &str) -> String {
    (0..40)
        .map(|i| {
            format!(
                "{indent}def method_{i}\n{indent}  compute_amount_{i}(fees, taxes, credits)\n{indent}end\n\n"
            )
        })
        .collect()
}

#[test]
fn a_rails_class_body_window_is_named_after_its_class() {
    let src = format!(
        "# An invoice issued to a customer\n# for one billing period.\n\
         class Invoice < ApplicationRecord\n  belongs_to :customer\n  has_many :fees\n  \
         validates :currency, inclusion: {{ in: currency_list }}\n  \
         scope :ready_to_be_finalized, -> {{ where(status: :draft) }}\n\n{}  \
         private\n\n  attr_reader :billing_period_boundaries\nend\n",
        ruby_methods("  ")
    );
    let chunks = SourceParser::parse(&src, "invoice.rb", "ruby").unwrap();

    let body = window_with(&chunks, "belongs_to :customer");
    assert_eq!(body.name.as_deref(), Some("Invoice"), "{body:#?}");
    assert_eq!(body.parent_scope, None, "{body:#?}");
    assert!(body.content.contains("scope :ready_to_be_finalized"));
    assert!(
        body.content.contains("An invoice issued to a customer"),
        "the class's doc comment opens its first window: {body:#?}"
    );
    assert_eq!(body.start_line, 1, "{body:#?}");

    let between_members = window_with(&chunks, "attr_reader :billing_period_boundaries");
    assert_eq!(between_members.name, None, "{between_members:#?}");

    let member = chunks
        .iter()
        .find(|c| c.name.as_deref() == Some("method_0"))
        .expect("the members are still chunked");
    assert_eq!(member.parent_scope.as_deref(), Some("class Invoice"));
}

#[test]
fn a_namespace_chain_stays_with_the_class_it_opens() {
    let src = format!(
        "# frozen_string_literal: true\n\nmodule Integrations\n  module Aggregator\n    \
         # Pushes a credit note to the accounting integration\n    # the customer is synced with.\n    \
         class CreateService < BaseService\n      Result = BaseResult[:credit_note]\n\n{}    \
         end\n  end\nend\n",
        ruby_methods("      ")
    );
    let chunks = SourceParser::parse(&src, "create_service.rb", "ruby").unwrap();

    let head = window_with(&chunks, "Result = BaseResult");
    assert_eq!(head.name.as_deref(), Some("CreateService"), "{head:#?}");
    assert!(head.content.contains("module Integrations"), "{head:#?}");
    assert_eq!(head.start_line, 1, "{head:#?}");
    assert!(
        !chunks
            .iter()
            .any(|c| c.name.as_deref() == Some("Integrations")),
        "a bare namespace line is not a window of its own: {chunks:#?}"
    );
}

#[test]
fn a_gap_crossing_a_container_boundary_is_cut_there() {
    let src = format!(
        "module Billing\n  SUPPORTED_CURRENCIES = %w[EUR USD GBP CHF].freeze\n\n  \
         class Invoice < ApplicationRecord\n    has_many :applied_taxes\n    \
         belongs_to :billing_entity\n\n{}    has_one :tax_summary_record\n  end\n\n  \
         ROUNDING_PRECISION_DIGITS = 6\nend\n",
        ruby_methods("    ")
    );
    let chunks = SourceParser::parse(&src, "billing/invoice.rb", "ruby").unwrap();

    let module_head = window_with(&chunks, "SUPPORTED_CURRENCIES");
    assert_eq!(
        module_head.name.as_deref(),
        Some("Billing"),
        "{module_head:#?}"
    );
    assert!(
        !module_head.content.contains("has_many"),
        "{module_head:#?}"
    );

    let class_head = window_with(&chunks, "has_many :applied_taxes");
    assert_eq!(
        class_head.name.as_deref(),
        Some("Invoice"),
        "{class_head:#?}"
    );
    assert!(!class_head.content.contains("SUPPORTED_CURRENCIES"));

    // `has_one` after the last method and the module constant after the
    // class's `end` form one uncovered stretch.
    let class_tail = window_with(&chunks, "has_one :tax_summary_record");
    assert!(
        !class_tail.content.contains("ROUNDING_PRECISION_DIGITS"),
        "{class_tail:#?}"
    );
    let module_tail = window_with(&chunks, "ROUNDING_PRECISION_DIGITS");
    assert!(!module_tail.content.contains("has_one"), "{module_tail:#?}");
}

#[test]
fn a_top_level_gap_stays_unnamed() {
    let src = format!(
        "require 'bigdecimal'\nrequire 'active_support/core_ext/numeric'\n\n\
         class Invoice < ApplicationRecord\n  has_many :credit_notes_applied\n\n{}end\n\n\
         Invoice.include(Billing::CurrencyFormatting)\n",
        ruby_methods("  ")
    );
    let chunks = SourceParser::parse(&src, "invoice.rb", "ruby").unwrap();

    let head = window_with(&chunks, "require 'bigdecimal'");
    assert_eq!(head.name, None, "{head:#?}");
    assert!(!head.content.contains("has_many"), "{head:#?}");

    let tail = window_with(&chunks, "Billing::CurrencyFormatting");
    assert_eq!(tail.name, None, "{tail:#?}");

    let body = window_with(&chunks, "has_many :credit_notes_applied");
    assert_eq!(body.name.as_deref(), Some("Invoice"), "{body:#?}");
}

#[test]
fn a_python_model_s_field_declarations_are_named_after_the_model() {
    let methods: String = (0..40)
        .map(|i| {
            format!("    def method_{i}(self):\n        return compute_amount_{i}(self.fees)\n\n")
        })
        .collect();
    let src = format!(
        "from django.db import models\n\n\n\
         class Invoice(models.Model):\n    \
         customer = models.ForeignKey(Customer, on_delete=models.CASCADE)\n    \
         total_amount_cents = models.BigIntegerField(default=0)\n\n{methods}"
    );
    let chunks = SourceParser::parse(&src, "billing/models.py", "python").unwrap();

    let fields = window_with(&chunks, "total_amount_cents");
    assert_eq!(fields.name.as_deref(), Some("Invoice"), "{fields:#?}");
    assert!(!fields.content.contains("from django"), "{fields:#?}");

    let import = window_with(&chunks, "from django.db import models");
    assert_eq!(import.name, None, "{import:#?}");
}

#[test]
fn a_typescript_class_s_fields_are_named_after_the_class() {
    let methods: String = (0..40)
        .map(|i| {
            format!("  method{i}(): number {{\n    return computeAmount{i}(this.fees)\n  }}\n\n")
        })
        .collect();
    let src = format!(
        "import {{ Currency }} from './currency'\n\n\
         export class InvoiceStore {{\n  private readonly pendingInvoices = new Map<string, Invoice>()\n  \
         static defaultCurrency: Currency = 'EUR'\n\n{methods}}}\n"
    );
    let chunks = SourceParser::parse(&src, "invoiceStore.ts", "typescript").unwrap();

    let fields = window_with(&chunks, "pendingInvoices");
    assert_eq!(fields.name.as_deref(), Some("InvoiceStore"), "{fields:#?}");
    assert!(!fields.content.contains("import"), "{fields:#?}");
}

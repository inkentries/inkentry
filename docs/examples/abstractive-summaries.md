# Example: abstractive chunk summaries with your own agent

`inkentry index` composes a deterministic, offline **structural** summary for
every chunk (docstring sentence, split symbol name, split callee names, salient
literals) and folds it into the embedding input. That summary bridges retrieval
vocabulary; it is not written prose.

If you want **abstractive** (LLM-written) summaries — one-sentence natural
language descriptions of what each chunk does — run your own agent over the
indexed chunks. inkentry does not call an LLM for this: reasoning stays with the
caller's model, and the only feature that reaches for an LLM is `memory harvest`.

## The data source: `plumbing cat-chunks`

`inkentry plumbing cat-chunks <file>` emits every indexed chunk for a file as
JSONL, one object per line, with the fields an agent needs to summarise:

```bash
inkentry plumbing cat-chunks src/indexer/chunker.rs
```

Each line carries the chunk's id, symbol name, kind, line range, and content.
Iterate the files you care about (`inkentry plumbing ls-files` lists the indexed
set) and feed each chunk to your agent.

## An example prompt

Give your agent one chunk at a time (or a small batch) with an instruction like:

> Summarise this code chunk in one sentence. Say what it does, not how. Reply
> with only the sentence, no preamble.
>
> ```
> {chunk.content}
> ```

Collect the `{chunk.id, summary}` pairs your agent produces.

## Writing summaries back (optional)

The `chunks.summary` column holds the structural summary that is embedded. It is
managed by the index pipeline and re-derived on the next `inkentry index`, so it
is **not** a place to store abstractive prose you want to keep — a re-index
would recompose it. Keep abstractive summaries in your own store (a sidecar
file, a memory entry, your agent's context) keyed by chunk id, and regenerate
them from `cat-chunks` when the code changes.

If you want an abstractive summary to inform retrieval, the durable path is a
`inkentry memory add --kind note` entry describing the area in the vocabulary
your queries use; memory is searched alongside code.

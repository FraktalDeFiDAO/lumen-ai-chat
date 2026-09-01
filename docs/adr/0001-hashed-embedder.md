# 1. Retrieval ships with a hashed embedder, not a trained model

Date: 2026-08-31

## Status

Accepted.

## Context

`search_memory` retrieves over an index whose vectors come from `HashEmbedder`,
a deterministic hashed bag-of-tokens. It ranks on shared vocabulary: a query
using the words the code uses finds it, a paraphrase sharing none of them does
not. It is not semantic, and for a while the tool description claimed it was —
corrected in `ea48935`, because a tool description is a prompt and a model
promised meaning writes prose queries, which is the input this index answers
worst.

Making it semantic means a sentence-transformer. That was costed rather than
assumed:

- **The runtime is a release candidate.** The Rust ONNX binding, `ort`, is at
  `2.0.0-rc.13`. Putting an RC inference runtime inside a container that runs
  with a read-only root and no capabilities is a different risk from putting it
  in a script.
- **The weights are ~90 MB.** Vendoring them puts 90 MB in git; fetching them at
  image build time puts a network dependency inside a build that currently has
  none, and a supply-chain question where there currently is not one. The API
  image is already 670 MB.
- **The tokenizer is already doing the work that is cheap.** It splits on
  non-alphanumerics and on camelCase, PascalCase, acronym and alpha/numeric
  boundaries, so `searchMemory`, `search_memory` and "search memory" already
  agree. The remaining gap is genuinely paraphrase, and nothing short of a model
  closes it.

## Decision

Ship the hashed embedder. Describe it accurately everywhere it is mentioned —
the tool description, the README and the settings card all say word overlap and
symbol matching rather than meaning.

Keep the swap cheap: `Embedder` is a trait, and the embedding width is pinned at
384, which is MiniLM's, so substituting a real model changes neither the on-disk
row format nor anything that reads it.

## Consequences

Retrieval is worse than a semantic index for a user who describes behaviour in
their own words, and no worse for one who uses the project's vocabulary. The
tool description steers toward the second.

Revisit when `ort` reaches a stable release, or if retrieval quality becomes the
thing users complain about. The evidence to gather first is which queries fail:
if they are paraphrases, this decision is wrong; if they are misspellings or
partial identifiers, a cheaper fix exists.

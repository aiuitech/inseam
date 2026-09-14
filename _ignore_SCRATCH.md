## WIP

## Todos
- Twilio integration

### Web UI
- ai skills for building inseam plugins, setting up nodes, and publishing to our plugin registry
- figure out how to fetch sources from node -> node.ios that contains things like photo index. 
  - if we can't: decide how to share indexes (at least the catalog) for sources that can only index locally (eg. ios photos)
- aggregate RAG testing data to run as benchmark
  - research current benchmark approaches
  - some ideas for our own way:
    - download large data set, include different content types
    - have a smart multi-modal LLM run over each file and generate: query data to match it
    - save these pairings as the answer key for best retrieval
    - grade on score proximity to ideal set from the LLM extracted queries
- goal: hook up an agent with inseam to allow it to visit files, get related content and merge/organize

## Maybe Later

- explore "dependency graphs" for discovery. We implicitly form dependency graphs in our mind before approaching how to solve a subject. That might be useful.

## Indexing

Think of it like Google search index, but instead a private local data set. "Backlinks" get covered by our graph transforms, keywords extracted automatically by LLM, and a global index of localized terms so that the nomenclature arises and ranks out of the local data only. Pagerank use the graph. LLM extraction gets contextual awareness of the words. Perhaps embed terms and group them by cosine similarity to form clusters of terms that are used to seed the term context to LLM when its extracting. Eg) embed the fragment, match with nearest term group's embedding, then pass that group to LLM during term/entity extraction so it doesn't extract irrelevant terms that never appear outside of the corpus.

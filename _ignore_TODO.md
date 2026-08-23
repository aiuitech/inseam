- indexing should be incremental so we don't have to generate everything for experiments that turn off entity fragments for example
- explore "dependency graphs" for discovery. We implicitly form dependency graphs in our mind before approaching how to solve a subject. That might be useful. 

## WIP

## Todos
- binary fragment handling. Eg) URL -> Image -> OCR. How to handle image?
- support fetching of binary data/files as well
- need folder as a source fragment
  - summarizer should be recursive. so at the folder level it's one summary of all its child files that each have their own summary. 
- query results should have meta data for time, etc
- support fetching of binary data/files as well

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

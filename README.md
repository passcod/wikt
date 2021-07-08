# wikt

Experimental playground for wiktionary data.

**This document might not update as often as the code does.**

## Set up

You'll want a minimum of 10 GB free space, a decent internet connection to download the dump (it's
about 1GB), an SSD (it's very dependent on I/O), a multi-core processor (it's very parallel). It
will work on a single core, just multiply every duration below by 8–10×.

### Install

As this is an experiment/playground it's probably best to clone this repo locally.

Then call the tool with `cargo run --release -- OPTIONS`. For brevity below I say `wikt OPTIONS` but
I'm actually using the cargo invocation.

Building without `--release` is somewhat faster but 40–100× slower to process data so really not
worth it.

A useful global option is `-V`, which takes a log level. Set to `debug` for a little more logging,
set to `trace` for a lot more logs including dumps of intermediate data, set to `warn` or `error` to
omit the default (`info`) logging.

### To get a dump:

1. https://dumps.wikimedia.org/enwiktionary/
2. Select the penultimate dated folder. Not the last one, which might be incomplete, the one before
    last. Or the last, if you're sure it's complete.
3. You want the `pages-articles-multistream-xml` file. Not the index file, we don't use that.
4. Download it and unpack it.

It might be helpful to keep the packed version around so you can just delete the unpacked file once
done with it, to save space and time. Alternatively, you can repack it with zstd, it will be faster
to decompress should you need to and take up about the same space. If you're working on btrfs you
can probably use transparent zstd compression for the same effect without having to unpack again.

The tooling is focussed on the english wiktionary, may work on other languages, may not.

### Do once: extract dump into the store

The dump is a single massive XML file. That's pretty much impossible to query or do anything with
in any kind of efficient.

So, the first step with `wikt` is to extract just the useful information into a custom binary format
which I designed to be trivial to parse but massively parallelisable, as well as having some random
access capability. That's stored in (by default) a `store` folder, and actually works out to a bit
smaller than the dump itself. At writing, my store folder contains 1472 ZStandard compressed files
of this custom format.

You generate the store with:

```
wikt store make path/to/dump.xml
```

This will take hours.

Each file in the store is called a "block", each block contains up to 10k "entries", which contain
the raw title and body of a wiktionary page. Blocks have a short header with the amount of entries
within and an array of byte offsets into the subsequent data section where each entry starts. Blocks
are zstd compressed by wikt, with a dictionary trained on the first block. Entries have a header
with two byte lengths, one each for the title and body data.

So you can read an entry given the name of the block and the number of the entry within that block.
That's expressed as a "ref" or "refid" which is two u32s separated by a slash in the human/textual
form, or by a u64 containing the concatenation of the two u32s in machine form.

And you can read all entries by iterating (in parallel) the entire `store` folder, and then opening
each block, decompressing it, and after parsing the block header, parsing every entry in parallel.

It could be made faster by decompressing only the block header, and then seeking to the required
position (for random access) or chunking along byte offsets and parsing each chunk from the zstd
stream (for sequential access). Also there might be facilities in the zstd format itself for that
pattern of use that we don't take advantage of currently.

### Query the store

You can search the store for substrings in the text of entries, or for negative matches. This is
fairly slow for specific queries because it literally iterates the entire store and runs the matcher
on every entry, but on my machine a single substring search takes ~30 seconds to run through it all,
so it's not that terrible.

You can query the store while it's extracting the dump.

```
wikt store query word "phrase with spaces" ~negative
```

Each entry returned is just the title prefixed by the refid in `[`brackets`]`, you can use that
to get the full text of the entry:

```
wikt store get 10000/1234
```

You can use the `--count` flag to instead return the amount of entries it matched, this is faster
simply by virtue of not having to write to output for every entry.

### Build the index

Once you've gotten a full store, you can build the index:

```
wikt index make
```

This will take 5-15 minutes.

This is one of the places where you can play around, by changing how the index is build. To make it
easier to see changes in effect, there's a `--limited N` option. Set `N` to e.g. 10, that will stop
after reading 10 blocks into the index.

Each entry is read _at least once_ into the index. A "document" is an indexed entry or subentry.
As of writing, the full index is ~7.3 million entries and indexes out to ~20 million documents.

The index has the title of each entry stored, so it can return titles fast, and the refid, so the
text of an entry can be fetched from the store. The full text of the entry is _not_ stored, which
saves considerable space. Still, as of writing the index was several gigabytes large.

### Query the index

You pass a Tantivy full text query, and it returns the top scored results.

```
wikt index query 'star system'
```

You can query phrases with `"phrase"`, and make a term requirement stronger with a `+` prefix, or
exclude a term with a `-` prefix. See Tantivy for more.

By default it searches in the body, and you can query specific fields with `field:expression`. For
example to get results of english nouns:

```
wikt index query '+lang:english +gram:noun'
```

Obviously the fields depend on how you built your index.

You can't query an index that was created with different fields than how you're querying it. So if
you make changes to the schema you'll need to rebuild the index before querying. Contrary to the
store, you can't query the index until changes are committed, and the `index make` process only
commits once at the end.

The query output contains a bit of metadata:

```
score=24.352657 [2440000/439] (english/?) double star system
        ===Noun=== {{en-noun|head=[[double]] [[star system]]}}  # {{lb|en|star}} a [[bi…
```

That's:
- the index search score
- the refid
- the lang/gram indicator (here english language, unset grammatical category)
- the title of the entry (in the actual output it's in bold)
- an excerpt (80 chars) of the entry

By default it fetches an excerpt of the text for display. You can have it show the entire entry with
`--full`. Or you can skip fetching the text, which will be faster, with `--titles`.

Use `-n` to change the number of results returned (default 20).
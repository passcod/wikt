use std::{
	collections::HashMap,
	fs::{create_dir_all, remove_dir_all, File},
	path::PathBuf,
	sync::atomic::{AtomicUsize, Ordering},
};

use blockstore::Ref;
use color_eyre::eyre::{eyre, Result};
use log::{debug, info, trace};
use once_cell::sync::Lazy;
use regex::Regex;
use structopt::StructOpt;
use tantivy::{
	collector::TopDocs,
	directory::MmapDirectory,
	doc,
	query::QueryParser,
	schema::{Schema, FAST, INDEXED, STORED, TEXT},
	DocAddress, Index, Score,
};

use xmldump::Page;

mod blockstore;
mod xmldump;

#[derive(StructOpt, Debug, Clone)]
struct Args {
	#[structopt(short = "V", long, default_value = "info")]
	pub log_level: log::Level,

	#[structopt(short = "S", long, default_value = "store")]
	pub store_dir: PathBuf,

	#[structopt(short = "I", long, default_value = "index")]
	pub index_dir: PathBuf,

	#[structopt(subcommand)]
	pub action: Action,
}

#[derive(StructOpt, Debug, Clone)]
enum Action {
	Store(StoreAction),
	Index(IndexAction),
}

#[derive(StructOpt, Debug, Clone)]
enum StoreAction {
	Make {
		dump: PathBuf,
	},

	Get {
		refid: Ref,
	},

	Query {
		searches: Vec<String>,

		#[structopt(long)]
		count: bool,
	},
}

#[derive(StructOpt, Debug, Clone)]
enum IndexAction {
	Make {
		#[structopt(long)]
		force: bool,

		/// only index N blocks (0 disables)
		#[structopt(short = "n", long, default_value = "0")]
		limited: usize,
	},

	Query {
		#[structopt(short = "n", long, default_value = "20")]
		limit: usize,

		// return only titles (ie don't read the store)
		#[structopt(long)]
		titles: bool,

		// return full entries instead of truncating
		#[structopt(long)]
		full: bool,

		search: String,
	},
}

fn main() -> Result<()> {
	color_eyre::install()?;

	let args = Args::from_args();

	stderrlog::new()
		.verbosity(match args.log_level {
			log::Level::Error => 0,
			log::Level::Warn => 1,
			log::Level::Info => 2,
			log::Level::Debug => 3,
			log::Level::Trace => 4,
		})
		.timestamp(stderrlog::Timestamp::Millisecond)
		.show_module_names(true)
		.module("wikt")
		.init()?;

	match args.action {
		Action::Store(StoreAction::Make { dump }) => {
			let mut store = blockstore::Store::new(args.store_dir);
			store.create()?;

			let dump = File::open(dump)?;
			let xml = xml::EventReader::new(dump);

			let mut n = 0;
			let mut current = Page::None;
			let mut block = blockstore::Block::default();

			for event in xml {
				let event = event?;

				current = Page::parse(current, event);
				match current {
					Page::Texted {
						ref title,
						ref text,
					} => {
						let entry = blockstore::Entry::new(title, text);
						block.add(entry)?;

						n += 1;
						print!("\x1b[2K\x1b[0G{}", n);
						if n % 10000 == 0 {
							println!(": commit");
							store.commit(&mut block, n)?;
						}
					}

					_ => {}
				}
			}

			println!(": commit");
			store.commit(&mut block, n)?;
			println!("{}! done.", n);
		}

		Action::Store(StoreAction::Get { refid }) => {
			let mut store = blockstore::Store::new(args.store_dir);
			store.open()?;
			let entry = store.read_entry(refid)?.open();
			println!("{}\n\n{}", entry.0, entry.1);
		}

		Action::Store(StoreAction::Query { searches, count }) => {
			use rayon::prelude::*;
			use std::sync::Arc;

			let mut store = blockstore::Store::new(args.store_dir);
			store.open()?;

			let blocks = store.blocks()?;
			let filtered = blocks
				.par_iter()
				.flat_map(|path| {
					let block = store.read_block(path).expect("error reading block");
					let block = Arc::new(block);
					(0..block.n).into_par_iter().map(move |n| {
						let block = block.clone();
						block.entry(n).expect("error parsing entry").open()
					})
				})
				.filter(move |(_, text, _)| {
					searches.iter().all(|search| {
						if search.starts_with('~') {
							!text.contains(search)
						} else {
							text.contains(search)
						}
					})
				});

			if count {
				println!("{}", filtered.count());
			} else {
				filtered.for_each(|(title, _, id)| println!("{}: {}", id, title));
			}
		}

		Action::Index(IndexAction::Make { force, limited }) => {
			if args.index_dir.exists() {
				if force {
					remove_dir_all(&args.index_dir)?;
				} else {
					return Err(eyre!(
						"index already exists, refusing to clobber without --force"
					));
				}
			}

			create_dir_all(&args.index_dir)?;

			let dir = MmapDirectory::open(args.index_dir)?;
			let schema = schema();
			let index = Index::open_or_create(dir, schema.clone())?;
			let mut index_writer = index.writer(100_000_000)?;

			use rayon::prelude::*;
			use std::sync::Arc;

			let mut store = blockstore::Store::new(args.store_dir);
			store.open()?;

			let mut blocks = store.blocks()?;
			if limited > 0 {
				blocks.truncate(limited);
			}

			let entries = blocks.par_iter().flat_map(|path| {
				let block = store.read_block(path).expect("error reading block");
				let block = Arc::new(block);
				(0..block.n).into_par_iter().map(move |n| {
					let block = block.clone();
					block.entry(n).expect("error parsing entry").open()
				})
			});

			let s_title = schema.get_field("title").unwrap();
			let s_text = schema.get_field("text").unwrap();
			let s_ref = schema.get_field("ref").unwrap();
			let s_lang = schema.get_field("lang").unwrap();
			let s_gram = schema.get_field("gram").unwrap();

			let n = Arc::new(AtomicUsize::new(0));

			info!("populating the index");
			entries.for_each(|(title, text, store_ref)| {
				let mut docs = Vec::with_capacity(10);

				for (name, text) in split_by_section(&LANG_RX, &text).into_iter() {
					debug!("[{}] lang={:?} section: {:?}", &store_ref, &name, &text);
					docs.push(doc!(
						s_title => title.as_str(),
						s_text => text.as_str(),
						s_ref => store_ref.as_u64(),
						s_lang => name.as_str(),
					));

					let lang = name;
					for (name, text) in split_by_section(&GRAM_RX, &text).into_iter() {
						debug!(
							"[{}] lang={:?} gram={:?} section: {:?}",
							&store_ref, &lang, &name, &text
						);
						docs.push(doc!(
							s_title => title.as_str(),
							s_text => text.as_str(),
							s_ref => store_ref.as_u64(),
							s_lang => lang.as_str(),
							s_gram => name.as_str(),
						));
					}
				}

				if docs.is_empty() {
					docs.push(doc!(
						s_title => title.as_str(),
						s_text => text.as_str(),
						s_ref => store_ref.as_u64(),
					));
				}

				for doc in docs {
					debug!("[{}] store document {:?}", &store_ref, doc);
					index_writer.add_document(doc);
				}

				let sofar = n.fetch_add(1, Ordering::Relaxed);
				if sofar % 10000 == 0 {
					info!("indexed {}k entries so far", sofar / 1000);
				}
			});

			info!("indexed {} entries", n.load(Ordering::Relaxed));
			info!("committing the index");
			index_writer.commit()?;
			info!(
				"index has {} documents",
				index
					.load_metas()?
					.segments
					.into_iter()
					.map(|m| m.num_docs())
					.sum::<u32>()
			);
		}

		Action::Index(IndexAction::Query {
			search,
			limit,
			titles,
			full,
		}) => {
			let mut store = blockstore::Store::new(args.store_dir);
			store.open()?;

			let index = Index::open_in_dir(args.index_dir)?;
			let reader = index.reader()?;
			let searcher = reader.searcher();

			let schema = schema();
			let s_text = schema.get_field("text").unwrap();

			let query_parser = QueryParser::for_index(&index, vec![s_text]);
			let query = query_parser.parse_query(&search)?;

			let top_docs: Vec<(Score, DocAddress)> =
				searcher.search(&query, &TopDocs::with_limit(limit))?;
			for (score, doc_address) in top_docs {
				let retrieved_doc = searcher.doc(doc_address)?;
				let nameddoc = schema.to_named_doc(&retrieved_doc).0;

				let rid = Ref::from_u64(nameddoc.get("ref").unwrap()[0].u64_value().unwrap());
				let lang = nameddoc.get("lang").and_then(|f| f[0].text());
				let gram = nameddoc.get("gram").and_then(|f| f[0].text());

				if titles {
					let title = nameddoc.get("title").unwrap()[0].text().unwrap();

					println!(
						"\x1b[2mscore={} [{}] ({}/{}) \x1b[0m\x1b[1m{}\x1b[0m",
						score,
						rid,
						lang.unwrap_or("?"),
						gram.unwrap_or("?"),
						title,
					);
				} else {
					let (title, mut text, _) = store.read_entry(rid)?.open();

					if let Some(lang) = lang {
						if let Some(sub) = split_by_section(&LANG_RX, &text).get(lang) {
							text = sub.to_owned();
						}
					}

					if let Some(gram) = gram {
						if let Some(sub) = split_by_section(&GRAM_RX, &text).get(gram) {
							text = sub.to_owned();
						}
					}

					if !full {
						text = text.replace("\n", " ");
						if text.len() > 80 {
							text.truncate(text.char_indices().nth(79).unwrap().0);
							text.push('…');
						}
					}

					println!(
						"\x1b[2mscore={} [{}] ({}/{}) \x1b[1m{}\x1b[0m\n\t{}",
						score,
						rid,
						lang.unwrap_or("?"),
						gram.unwrap_or("?"),
						title,
						text,
					);
				}
			}
		}
	}

	Ok(())
}

fn schema() -> Schema {
	let mut schema_builder = Schema::builder();
	schema_builder.add_text_field("title", TEXT | STORED);
	schema_builder.add_text_field("text", TEXT);
	schema_builder.add_u64_field("ref", INDEXED | STORED | FAST);
	schema_builder.add_text_field("lang", TEXT | STORED);
	schema_builder.add_text_field("gram", TEXT | STORED);
	schema_builder.build()
}

static LANG_RX: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^\s*==([\w\s]+)==\s*$").unwrap());

static GRAM_RX: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^\s*===([\w\s]+)===\s*$").unwrap());

fn split_by_section(rx: &Regex, text: &str) -> HashMap<String, String> {
	let mut positions = Vec::with_capacity(10);
	for cap in rx.captures_iter(&text) {
		trace!("section capture: {:?}", cap);

		let whole = cap.get(0).unwrap();
		let name = cap.get(1).unwrap().as_str().to_lowercase();
		trace!("section name: {:?}", name);

		positions.push((name, whole.start(), whole.end()));
	}

	positions
		.iter()
		.enumerate()
		.map(|(i, (name, _, start))| {
			let end = positions
				.get(i + 1)
				.map(|(_, start, _)| *start)
				.unwrap_or_else(|| text.len() - 1);

			trace!("section part name={:?} start={} end={}", name, start, end);
			(
				name.to_owned(),
				text.chars()
					.skip(start - 1)
					.take(end - start)
					.collect::<String>()
					.trim()
					.to_owned(),
			)
		})
		.collect()
}



use std::fmt::Debug;
use std::rc::Rc;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::stream::FusedStream;
use futures::{Sink, SinkExt, Stream, StreamExt};
use log::{debug, trace};

use crate::terminal::ConsoleCtx;
use crate::term::{Drivable, KeyedCache, KeyedCacheRef};

const CACHE_CAPACITY: usize = 256;

/// Responsible for encapuslating and maintaining a connection to a CML device
/// Primary use case is deliniating data from the device, and signaling if a prompt is ready/has been found
#[derive(Debug)]
pub struct ConsoleDriver {
	ctx: Rc<ConsoleCtx>,
	conn: Box<dyn Drivable>,
	// connection state metadata

	/// How many data chunks we have recieved
	received_chunks: usize,

	data_cache: KeyedCache,
}

impl ConsoleDriver {
	// Getters
	pub fn context(&self) -> &ConsoleCtx { &self.ctx }

	/// Initializes the driver so it can manage a connection and provide status updates on the prompt context/etc
	pub fn from_connection(console: ConsoleCtx, conn: Box<dyn Drivable>) -> ConsoleDriver {
		ConsoleDriver {
			ctx: Rc::new(console),
			conn,

			// connection state metadata
			received_chunks: 0,
			data_cache: KeyedCache::with_capacity(CACHE_CAPACITY),
		}
	}

	/// Finds a prompt (suitable for the current device) and returns it
	fn find_prompt<'a>(&self, data: &'a [u8]) -> Option<(&'a str, bool)> {
		let node_def = &self.ctx.node().meta().node_definition;
		if let Ok(s) = std::str::from_utf8(data) {
			let prompt = s.lines()
				// get the last non-empty string
				.map(|s| s.trim())
				.filter_map(|s| if s.len() > 0 { Some(s) } else { None })
				.filter_map(|s| { // validate it as a proper hostname

					// try to validate hostnames for the specific machine types
					if node_def.is_ios() {
						// length: unlimited? (up to 99 shown on enable prompt, truncated for config/etc)

						let prompt = s.find(|c| c == '>' || c == '#').map(|i| &s[..=i])?;
						// 63 prompt len + prompt ending char + configuration mode
						if !( 2 <= prompt.len() && prompt.len() <= 63+1+(2+16) ) { return None; }
						let prompt_text = &prompt[..prompt.len()-1];

						// IOS seems to be more permissive
						if ! prompt_text.chars().all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | '(' | ')')) { return None; }

						Some(prompt)
					} else if node_def.is_asa() {
						// length: 63 chars (can go higher for `(config)#` etc)
						// start/end: letter/digit
						// interior: letter/digit/hyphen

						let prompt = s.find(|c| c == '>' || c == '#').map(|i| &s[..=i])?;
						// 63 prompt len + prompt ending char + configuration mode
						if !( 2 <= prompt.len() && prompt.len() <= 63+1+(2+16) ) { return None; }
						let prompt_text = &prompt[..prompt.len()-1];

						if ! prompt_text.starts_with(|c: char| c.is_alphanumeric()) { return None; }
						if ! prompt_text.ends_with(|c: char| c.is_alphanumeric() || c == ')') { return None; }

						// ensure the middle of the prompt contains only alphanumeric, and select characters
						if ! prompt_text.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '(' | ')')) { return None; }

						Some(prompt)
					} else if node_def.is_linux() {
						// do very little processing since they can vary so widely
						let prompt = s.find(|c| c == '$' || c == '#').map(|i| &s[..=i])?;
						if prompt.len() > 100 { return None; }
						Some(prompt)
					} else { // unknown
						// unknown device - use linux "permissiveness"
						let prompt = s.find(|c| c == '>' || c == '$' || c == '#').map(|i| &s[..=i])?;
						if prompt.len() > 100 { return None; }
						Some(prompt)
					}
				})
				.last();
	
			if let Some(prompt) = prompt {
				Some(( prompt, s.trim_end().ends_with(prompt) ))
			} else {
				None
			}
		} else {
			debug!("data chunk was not UTF8, skipping prompt detection");
			None
		}
	}

	fn handle_data_chunk(&mut self, chunk: Vec<u8>) -> ConsoleUpdate {
		let was_first = self.received_chunks == 0;
		self.received_chunks += 1;

		// update rolling buffer
		let cache_ref = self.data_cache.try_update(&chunk).unwrap();
		let last_prompt = {
			let cache = cache_ref.try_borrow().unwrap();

			// try to find a prompt
			let prompt_data = self.find_prompt(&cache);
			debug!("detected prompt for {:?}: {:?}", &self.ctx.node().meta().node_definition, prompt_data);
			prompt_data.map(|(s, b)| (s.to_owned(), b))
		};

		ConsoleUpdate {
			last_chunk: chunk,
			last_prompt,
			cache_ref,
			was_first,
		}
	}
}

#[derive(Debug)]
pub struct ConsoleUpdate {
	pub last_chunk: Vec<u8>,
	pub last_prompt: Option<(String, bool)>,
	pub was_first: bool,
	pub cache_ref: KeyedCacheRef,
}

impl FusedStream for ConsoleDriver {
	fn is_terminated(&self) -> bool {
		self.conn.is_terminated()
	}
}
impl Stream for ConsoleDriver {
	type Item = Result<ConsoleUpdate, crate::term::BoxedDriverError>;
	fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {

		// bubble pending/error
		// log on stream close/Ready(None)
		// map data chunks for prompt/etc
		self.conn.poll_next_unpin(cx)
			.map(|opt| {
				if let None = opt {
					trace!("inner console connection has closed");
				}
				opt.map(|res| {
					trace!("handling backend console chunk: {:?}", res);
					res.map(|odata| self.handle_data_chunk(odata))
				})
			})
	}
}

impl Sink<String> for ConsoleDriver {
	type Error = crate::term::BoxedDriverError;
	fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.conn.poll_ready_unpin(cx)
	}
	fn start_send(mut self: Pin<&mut Self>, item: String) -> Result<(), Self::Error> {
		self.conn.start_send_unpin(item)
	}
	fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.conn.poll_flush_unpin(cx)
	}
	fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		let res = self.conn.poll_close_unpin(cx);
		if let Poll::Ready(Ok(())) = res {
			debug!("Closed connection sink");
		}
		res
	}
}

mod bufferedstream {
	use std::io::{self, Read, Write};
	#[derive(Debug)]
	pub struct BufferedStream<T: Read + Write> {
		stream: io::BufReader<T>,
	}
	impl<T: Read + Write> BufferedStream<T> {
		pub fn new(stream: T) -> Self {
			Self {
				stream: io::BufReader::new(stream),
			}
		}

		pub fn write(&mut self) -> BufferedStreamWriter<T> {
			BufferedStreamWriter(io::BufWriter::new(self))
		}

		pub fn get_ref(&self) -> &T {
			self.stream.get_ref()
		}

		pub fn get_mut(&mut self) -> &mut T {
			self.stream.get_mut()
		}
	}
	impl<T: Read + Write> Read for BufferedStream<T> {
		fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
			self.stream.read(buf)
		}
	}
	impl<'a, T: Read + Write + 'a> Write for &'a mut BufferedStream<T> {
		fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
			self.stream.get_mut().write(buf)
		}

		fn flush(&mut self) -> io::Result<()> {
			self.stream.get_mut().flush()
		}
	}
	#[derive(Debug)]
	pub struct BufferedStreamWriter<'a, T: Read + Write + 'a>(
		io::BufWriter<&'a mut BufferedStream<T>>,
	);
	impl<'a, T: Read + Write + 'a> Write for BufferedStreamWriter<'a, T> {
		fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
			self.0.write(buf)
		}

		fn flush(&mut self) -> io::Result<()> {
			self.0.flush()
		}
	}
	impl<'a, T: Read + Write + 'a> Drop for BufferedStreamWriter<'a, T> {
		fn drop(&mut self) {
			self.0.flush().unwrap();
		}
	}
}
pub use self::bufferedstream::BufferedStream;

//////////////////////////////////////////////////////////////////////////////////////////////////////////////////

mod to_hex {
	use std::fmt;
	#[derive(Clone, Debug)]
	pub struct Hex<'a>(&'a [u8], bool);
	impl<'a> Iterator for Hex<'a> {
		type Item = char;

		fn next(&mut self) -> Option<char> {
			if !self.0.is_empty() {
				const CHARS: &[u8] = b"0123456789abcdef";
				let byte = self.0[0];
				let second = self.1;
				if second {
					self.0 = self.0.split_first().unwrap().1;
				}
				self.1 = !self.1;
				Some(CHARS[if !second { byte >> 4 } else { byte & 0xf } as usize] as char)
			} else {
				None
			}
		}
	}
	impl<'a> fmt::Display for Hex<'a> {
		fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
			for char_ in self.clone() {
				write!(f, "{}", char_)?;
			}
			Ok(())
		}
	}
	pub trait ToHex {
		fn to_hex(&self) -> Hex;
	}
	impl ToHex for [u8] {
		fn to_hex(&self) -> Hex {
			Hex(&*self, false)
		}
	}
}
pub use self::to_hex::ToHex;

//////////////////////////////////////////////////////////////////////////////////////////////////////////////////

mod rand_stream {
	use rand;
	#[derive(Debug)]
	pub struct Rand<T> {
		res: Option<T>,
		count: usize,
	}
	impl<T> Rand<T> {
		pub fn new() -> Self {
			Self {
				res: None,
				count: 0,
			}
		}

		pub fn push<R: rand::Rng>(&mut self, x: T, rng: &mut R) {
			self.count += 1;
			if rng.gen_range(0, self.count) == 0 {
				self.res = Some(x);
			}
		}

		pub fn get(self) -> Option<T> {
			self.res
		}
	}
	impl<T> Default for Rand<T> {
		fn default() -> Self {
			Self::new()
		}
	}
}
pub use self::rand_stream::Rand;

//////////////////////////////////////////////////////////////////////////////////////////////////////////////////

pub fn parse_binary_size(input: &str) -> Result<u64, ()> {
	let mut index = 0;
	if index == input.len() {
		return Err(());
	}
	index = input
		.chars()
		.position(|c| !c.is_ascii_digit())
		.unwrap_or(input.len());
	let a: u64 = input[..index].parse().unwrap();
	if index == input.len() {
		return Ok(a);
	}
	let b: u64 = if input[index..=index].chars().nth(0).ok_or(())? == '.' {
		index += 1;
		let _zeros = input[index..].chars().position(|c| c != '0').unwrap_or(0);
		let index1 = index;
		index = index + input[index..]
			.chars()
			.position(|c| !c.is_ascii_digit())
			.unwrap_or(input.len() - index);
		if index != index1 {
			input[index1..index].parse().unwrap()
		} else {
			0
		}
	} else {
		0
	};
	if index == input.len() {
		return Ok(a);
	}
	let c: u64 = match &input[index..] {
		"B" => 1,
		"KiB" => 1024,
		"MiB" => 1024_u64.pow(2),
		"GiB" => 1024_u64.pow(3),
		"TiB" => 1024_u64.pow(4),
		"PiB" => 1024_u64.pow(5),
		"EiB" => 1024_u64.pow(6),
		_ => return Err(()),
	};
	if b > 0 {
		unimplemented!();
	}
	Ok(a * c)
}

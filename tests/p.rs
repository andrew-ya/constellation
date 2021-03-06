//= {
//=   "output": {
//=     "2": [
//=       "",
//=       true
//=     ],
//=     "1": [
//=       "hi\nhi\n",
//=       true
//=     ]
//=   },
//=   "children": [
//=     {
//=       "output": {
//=         "2": [
//=           "",
//=           true
//=         ],
//=         "1": [
//=           "",
//=           true
//=         ]
//=       },
//=       "children": [],
//=       "exit": "Success"
//=     },
//=     {
//=       "output": {
//=         "2": [
//=           "",
//=           true
//=         ],
//=         "1": [
//=           "",
//=           true
//=         ]
//=       },
//=       "children": [],
//=       "exit": "Success"
//=     }
//=   ],
//=   "exit": "Success"
//= }

#![deny(warnings, deprecated)]
extern crate constellation;
#[macro_use]
extern crate serde_closure;
use constellation::*;

fn main() {
	init(Resources {
		mem: 20 * 1024 * 1024,
		..Resources::default()
	});
	for _ in 0..2 {
		let pid = spawn(
			Resources {
				mem: 20 * 1024 * 1024,
				..Resources::default()
			},
			FnOnce!(|parent| {
				let sender = Sender::<String>::new(parent);
				sender.send(String::from("hi"));
			}),
		)
		.expect("SPAWN FAILED");
		let receiver = Receiver::<String>::new(pid);
		println!("{}", receiver.recv().unwrap());
	}
}

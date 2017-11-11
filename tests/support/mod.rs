use std::collections::VecDeque;
use std::env;
use std::io::{self, Read, Write};
use std::str;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};

use jsonrpc_core::request::{Call, MethodCall, Notification};
use jsonrpc_core::version::Version;
use jsonrpc_core::params::Params;
use jsonrpc_core::response::{Output, Response};
use jsonrpc_core::id::Id;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::ser::Serializer;
use serde_json::{from_str, from_value, to_value, Value};

use url::Url;

use languageserver_types::{DidOpenTextDocumentParams, TextDocumentItem};

use gluon_language_server::rpc::read_message;

extern crate gluon;
use self::gluon::new_vm;

pub fn test_url(uri: &str) -> Url {
    Url::from_file_path(&env::current_dir().unwrap().join(uri)).unwrap()
}

pub fn write_message<W, V>(writer: &mut W, value: V) -> io::Result<()>
where
    W: ?Sized + Write,
    V: Serialize,
{
    let mut vec = Vec::new();
    value.serialize(&mut Serializer::new(&mut vec)).unwrap();
    write!(
        writer,
        "Content-Length: {}\r\n\r\n{}",
        vec.len(),
        str::from_utf8(&vec).unwrap()
    )
}

pub fn method_call<T>(method: &str, id: u64, value: T) -> Call
where
    T: Serialize,
{
    let value = to_value(value);
    let params = match value {
        Ok(Value::Object(map)) => Params::Map(map),
        _ => panic!("Expected map"),
    };
    Call::MethodCall(MethodCall {
        jsonrpc: Some(Version::V2),
        method: method.into(),
        id: Id::Num(id),
        params: Some(params),
    })
}

pub fn notification<T>(method: &str, value: T) -> Call
where
    T: Serialize,
{
    let value = to_value(value);
    let params = match value {
        Ok(Value::Object(map)) => Params::Map(map),
        _ => panic!("Expected map"),
    };
    Call::Notification(Notification {
        jsonrpc: Some(Version::V2),
        method: method.into(),
        params: Some(params),
    })
}

pub fn did_open_uri<W: ?Sized>(stdin: &mut W, uri: Url, text: &str)
where
    W: Write,
{
    let did_open = notification(
        "textDocument/didOpen",
        DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri,
                language_id: Some("gluon".into()),
                text: text.into(),
                version: Some(1),
            },
        },
    );

    write_message(stdin, did_open).unwrap();
}

pub fn did_open<W: ?Sized>(stdin: &mut W, uri: &str, text: &str)
where
    W: Write,
{
    did_open_uri(stdin, test_url(uri), text)
}


pub fn send_rpc<F, T>(f: F) -> T
where
    F: FnOnce(&mut Write),
    T: DeserializeOwned,
{
    let (stdin_read, mut stdin_write) = pipe();
    let (mut stdout_read, stdout_write) = pipe();

    ::std::thread::spawn(move || {
        let thread = new_vm();

        ::gluon_language_server::start_server(thread, stdin_read, stdout_write).unwrap();
    });

    {
        let mut stdin = &mut stdin_write;

        f(stdin);

        let exit = Call::Notification(Notification {
            jsonrpc: Some(Version::V2),
            method: "exit".into(),
            params: None,
        });
        write_message(&mut stdin, exit).unwrap();
    }

    let mut stdout = Vec::new();
    stdout_read.read_to_end(&mut stdout).unwrap();

    let mut value = None;
    let mut output = &stdout[..];
    while let Some(json) = read_message(&mut output).unwrap() {
        if let Ok(Response::Single(Output::Success(response))) = from_str(&json) {
            value = from_value(response.result).ok();
        }
        if let Ok(Notification {
            params: Some(params),
            ..
        }) = from_str(&json)
        {
            let json_value = match params {
                Params::Map(map) => Value::Object(map),
                Params::Array(array) => Value::Array(array),
                Params::None => Value::Null,
            };
            value = from_value(json_value).ok();
        }
    }
    value.unwrap_or_else(|| {
        panic!(
            "Could not find the expected response out of:\n`{}`",
            str::from_utf8(&stdout).expect("UTF8")
        )
    })
}


pub struct ReadPipe {
    recv: Receiver<Vec<u8>>,
    buffer: VecDeque<u8>,
}

impl Drop for ReadPipe {
    fn drop(&mut self) {
        "123".to_owned();
    }
}

impl Drop for WritePipe {
    fn drop(&mut self) {
        "123".to_owned();
    }
}


impl io::Read for ReadPipe {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let l = buffer.len().min(self.buffer.len());
        for (to, from) in buffer.iter_mut().zip(self.buffer.drain(..l)) {
            *to = from;
        }
        match self.recv.try_recv() {
            Ok(buf) => {
                self.buffer.extend(buf);
                let extra = self.read(&mut buffer[l..])?;
                Ok(l + extra)
            }
            Err(TryRecvError::Disconnected) => Err(io::Error::from(io::ErrorKind::NotConnected)),
            Err(_) => Ok(l),
        }
    }

    fn read_to_end(&mut self, buffer: &mut Vec<u8>) -> io::Result<usize> {
        let len = buffer.len();
        buffer.extend(self.buffer.drain(..));
        while let Ok(buf) = self.recv.recv() {
            buffer.extend(buf);
        }
        Ok(buffer.len() - len)
    }
}

pub struct WritePipe {
    sender: SyncSender<Vec<u8>>,
}

impl io::Write for WritePipe {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.sender.send(data.to_owned()).map_err(|_| {
            eprintln!("{:?}", str::from_utf8(data));
            io::Error::from(io::ErrorKind::NotConnected)
        })?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}


fn pipe() -> (ReadPipe, WritePipe) {
    let (sender, receiver) = sync_channel(10);
    (
        ReadPipe {
            recv: receiver,
            buffer: VecDeque::new(),
        },
        WritePipe { sender },
    )
}

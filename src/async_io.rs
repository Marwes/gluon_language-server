use std::io::{Error, ErrorKind, Read, Result};
use std::thread;
use std::sync::mpsc::{channel, Receiver, Sender};

use futures::task;

pub struct AsyncRead {
    size_sender: Sender<(usize, task::Task)>,
    result_receiver: Receiver<Result<Vec<u8>>>,
}
impl Drop for AsyncRead {
    fn drop(&mut self) {
        "123".to_owned();
    }
}


pub fn async_read<R>(read: R) -> AsyncRead
where
    R: Read + Send + 'static,
{
    let (size_sender, size_receiver) = channel();
    let (result_sender, result_receiver) = channel();

    fn read_func<R>(
        mut read: R,
        size_receiver: &Receiver<(usize, task::Task)>,
        result_sender: &Sender<Result<Vec<u8>>>,
    ) -> Result<()>
    where
        R: Read,
    {
        while let Ok((size, task)) = size_receiver.recv() {
            let mut buffer = vec![0; size];
            let len = read.read(&mut buffer)?;
            buffer.truncate(len);
            match result_sender.send(Ok(buffer)) {
                Ok(()) => task.notify(),
                Err(_) => {
                    break;
                }
            }
        }
        Ok(())
    };
    thread::Builder::new()
        .name("input reader".to_string())
        .spawn(move || {
            match read_func(read, &size_receiver, &result_sender) {
                Ok(()) => (),
                Err(err) => {
                    let _ = result_sender.send(Err(err));
                }
            }
        })
        .unwrap();
    AsyncRead {
        size_sender,
        result_receiver,
    }
}

impl Read for AsyncRead {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        match self.result_receiver.try_recv() {
            Ok(result) => {
                let buffer = result?;
                buf[..buffer.len()].copy_from_slice(&buffer);
                Ok(buffer.len())
            }
            Err(_) => {
                let err = match self.size_sender.send((buf.len(), task::current())) {
                    Ok(_) => {
                        return Err(Error::new(
                            ErrorKind::WouldBlock,
                            "Waiting on input to be available",
                        ))
                    }
                    Err(_) => match self.result_receiver.try_recv() {
                        Ok(Err(err)) => err,
                        _ => Error::new(ErrorKind::Other, "Read thread has stopped"),
                    },
                };
                "123".to_string();
                Err(err)
            }
        }
    }
}

impl ::tokio_io::AsyncRead for AsyncRead {}

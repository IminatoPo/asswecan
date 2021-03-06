use cmd::{Cmd, CmdType, Command, RESP_OBJ_ERROR_NOT_SUPPORT};
use com::*;
use resp::Resp;
use tokio::prelude::{Async, AsyncSink, Future, Sink, Stream};
use Cluster;

const MAX_CONCURRENCY: usize = 1024 * 8;
// use aho_corasick::{AcAutomaton, Automaton, Match};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

pub struct Handle<I, O>
where
    I: Stream<Item = Resp, Error = Error>,
    O: Sink<SinkItem = Cmd, SinkError = Error>,
{
    cluster: Rc<Cluster>,

    input: I,
    output: O,
    cmds: VecDeque<Cmd>,
    count: usize,
    waitq: VecDeque<Cmd>,
}

impl<I, O> Handle<I, O>
where
    I: Stream<Item = Resp, Error = Error>,
    O: Sink<SinkItem = Cmd, SinkError = Error>,
{
    pub fn new(cluster: Rc<Cluster>, input: I, output: O) -> Handle<I, O> {
        Handle {
            cluster: cluster,
            input: input,
            output: output,
            cmds: VecDeque::new(),
            count: 0,
            waitq: VecDeque::new(),
        }
    }

    fn try_read(&mut self) -> Result<Async<Option<()>>, Error> {
        loop {
            if self.cmds.len() > MAX_CONCURRENCY {
                return Ok(Async::NotReady);
            }

            match try_ready!(self.input.poll()) {
                Some(val) => {
                    let cmd = Command::from_resp(val);
                    let is_complex = cmd.is_complex();
                    let rc_cmd = Rc::new(RefCell::new(cmd));
                    self.cmds.push_back(rc_cmd.clone());
                    if is_complex {
                        for sub in rc_cmd
                            .borrow()
                            .sub_reqs
                            .as_ref()
                            .cloned()
                            .expect("never be empty")
                        {
                            self.waitq.push_back(sub);
                        }
                    } else {
                        self.waitq.push_back(rc_cmd);
                    }
                }
                None => {
                    return Ok(Async::Ready(None));
                }
            }
        }
    }

    fn try_send(&mut self) -> Result<Async<()>, Error> {
        loop {
            if self.waitq.is_empty() {
                return Ok(Async::NotReady);
            }

            let rc_cmd = self
                .waitq
                .front()
                .cloned()
                .expect("front of waitq is never be None");

            let cmd_type = rc_cmd.borrow().get_cmd_type();
            match cmd_type {
                CmdType::NotSupport | CmdType::Ctrl => {
                    rc_cmd
                        .borrow_mut()
                        .done_with_error(&RESP_OBJ_ERROR_NOT_SUPPORT);
                    let _ = self.waitq.pop_front().unwrap();
                    continue;
                }
                _ => {}
            }

            match self.cluster.dispatch(rc_cmd)? {
                AsyncSink::NotReady(_) => {
                    return Ok(Async::NotReady);
                }
                AsyncSink::Ready => {
                    let _ = self.waitq.pop_front().unwrap();
                }
            }
        }
    }

    fn try_write(&mut self) -> Result<Async<()>, Error> {
        let ret: Result<Async<()>, Error> = Ok(Async::NotReady);
        loop {
            if self.cmds.is_empty() {
                break;
            }

            let rc_cmd = self.cmds.front().cloned().expect("cmds is never be None");
            if !rc_cmd.borrow().is_done() {
                break;
            }

            match self.output.start_send(rc_cmd)? {
                AsyncSink::NotReady(_) => {
                    break;
                }
                AsyncSink::Ready => {
                    let _ = self.cmds.pop_front().unwrap();
                    self.count += 1;
                }
            }
        }

        if self.count > 0 {
            try_ready!(self.output.poll_complete());
            self.count = 0;
        }

        ret
    }
}

impl<I, O> Future for Handle<I, O>
where
    I: Stream<Item = Resp, Error = Error>,
    O: Sink<SinkItem = Cmd, SinkError = Error>,
{
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Result<Async<Self::Item>, Self::Error> {
        let mut can_read = true;
        let mut can_send = true;
        let mut can_write = true;

        loop {
            if !(can_read && can_send && can_write) {
                return Ok(Async::NotReady);
            }

            // step 1: poll read from input stream.
            if can_read {
                // read until the input stream is NotReady.
                match self.try_read()? {
                    Async::NotReady => {
                        can_read = false;
                    }
                    Async::Ready(None) => {
                        return Ok(Async::Ready(()));
                    }
                    Async::Ready(Some(())) => {}
                }
            }

            // step 2: send to cluster.
            if can_send {
                // send until the output stream is unsendable.
                match self.try_send()? {
                    Async::NotReady => {
                        can_send = false;
                    }
                    Async::Ready(_) => {}
                }
            }

            // step 3: wait all the cluster is done.
            if can_write {
                // step 4: poll send back to client.
                match self.try_write()? {
                    Async::NotReady => {
                        can_write = false;
                    }
                    Async::Ready(_) => {}
                }
            }
        }
        // Ok(Async::NotReady)
    }
}

// Copyright 2018 Google Inc. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod view;

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

use serde_json::{self, Value};

use xi_core::{ViewIdentifier, PluginPid, ConfigTable};
use xi_core::plugin_rpc::{PluginBufferInfo, PluginUpdate, HostRequest, HostNotification};
use xi_rpc::{self, RpcLoop, RpcCtx, RemoteError, ReadError, Handler as RpcHandler};
use self::view::{Plugin, View, Cache};

macro_rules! bail {
    ($opt:expr, $method:expr, $pid:expr, $view:expr) => ( match $opt {
        Some(t) => t,
        None => {
            eprintln!("{:?} missing {:?} for {:?}", $pid, $view, $method);
            return
        }
    })
}

macro_rules! bail_err {
    ($opt:expr, $method:expr, $pid:expr, $view:expr, $err:expr) => ( match $opt {
        Some(t) => t,
        None => {
            eprintln!("{:?} missing {:?} for {:?}", $pid, $view, $method);
            return Err($err)
        }
    })
}

/// Handles raw RPCs from core, updating documents and bridging calls
/// to the plugin,
pub struct Dispatcher<'a, P: 'a + Plugin> {
    //TODO: when we add multi-view, this should be an Arc+Mutex/Rc+RefCell
    views: HashMap<ViewIdentifier, View<P::Cache>>,
    pid: Option<PluginPid>,
    plugin: &'a mut P,
}

impl<'a, P: 'a + Plugin> Dispatcher<'a, P> {
    pub fn new(plugin: &'a mut P) -> Self {
        Dispatcher {
            views: HashMap::new(),
            pid: None,
            plugin: plugin,
        }
    }

    fn do_initialize(&mut self, ctx: &RpcCtx,
                     plugin_id: PluginPid,
                     buffers: Vec<PluginBufferInfo>)
    {
        assert!(self.pid.is_none(), "initialize rpc received with existing pid");
        self.pid = Some(plugin_id);
        self.do_new_buffer(ctx, buffers);

    }

    fn do_did_save(&mut self, view_id: ViewIdentifier, path: PathBuf) {
        let v = bail!(self.views.get_mut(&view_id), "did_save", self.pid, view_id);
        self.plugin.did_save(v, &path);
        v.path = Some(path);
    }

    fn do_config_changed(&mut self, view_id: ViewIdentifier, changes: ConfigTable) {
        let v = bail!(self.views.get_mut(&view_id), "config_changed", self.pid, view_id);
        self.plugin.config_changed(v, &changes);
    }

    fn do_new_buffer(&mut self, ctx: &RpcCtx, buffers: Vec<PluginBufferInfo>) {
        let plugin_id = self.pid.unwrap();
        buffers.into_iter()
            .map(|info| View::new(ctx.get_peer().clone(), plugin_id, info))
            .for_each(|view| {
                let mut view = view;
                self.plugin.new_view(&mut view);
                self.views.insert(view.view_id, view);
            });

    }

    fn do_close(&mut self, view_id: ViewIdentifier) {
        {
            let v = bail!(self.views.get(&view_id), "close", self.pid, view_id);
            self.plugin.did_close(v);
        }
        self.views.remove(&view_id);
    }

    fn do_update(&mut self, update: PluginUpdate) -> Result<Value, RemoteError> {
        let PluginUpdate {
            view_id, delta, new_len, new_line_count, rev, edit_type, author,
        } = update;
        let v = bail_err!(self.views.get_mut(&view_id), "update",
                          self.pid, view_id,
                          RemoteError::custom(404, "missing view", None));
        v.cache.update(delta.as_ref(), new_len, new_line_count, rev);
        self.plugin.update(v, delta.as_ref())
    }

    fn do_shutdown(&mut self) {
        //TODO: handle shutdown

    }

    fn do_tracing_config(&mut self, enabled: bool) {
        use xi_trace;

        if enabled {
            eprintln!("Enabling tracing in {:?}", self.pid);
            xi_trace::enable_tracing();
        } else {
            eprintln!("Disabling tracing in {:?}",  self.pid);
            xi_trace::disable_tracing();
        }
    }
}

impl<'a, P: Plugin> RpcHandler for Dispatcher<'a, P> {
    type Notification = HostNotification;
    type Request = HostRequest;

    fn handle_notification(&mut self, ctx: &RpcCtx, rpc: Self::Notification) {
        use self::HostNotification::*;
        match rpc {
            Initialize { plugin_id, buffer_info } =>
                self.do_initialize(ctx, plugin_id, buffer_info),
            DidSave { view_id, path } =>
                self.do_did_save(view_id, path),
            ConfigChanged { view_id, changes } =>
                self.do_config_changed(view_id, changes),
            NewBuffer { buffer_info } =>
                self.do_new_buffer(ctx, buffer_info),
            DidClose { view_id } =>
                self.do_close(view_id),
            //TODO: figure out shutdown
            Shutdown ( .. ) =>
                self.do_shutdown(),
            TracingConfig { enabled } =>
                self.do_tracing_config(enabled),
            Ping ( .. ) => (),
        }
    }

    fn handle_request(&mut self, ctx: &RpcCtx, rpc: Self::Request)
                      -> Result<Value, RemoteError> {
        use self::HostRequest::*;
        match rpc {
            Update(params) =>
                self.do_update(params),
            CollectTrace ( .. ) =>
                Err(RemoteError::custom(100, "method not supported", None)),
        }
    }

    fn idle(&mut self, _ctx: &RpcCtx, token: usize) {
        let view_id: ViewIdentifier = token.into();
        let v = bail!(self.views.get_mut(&view_id), "idle", self.pid, view_id);
        self.plugin.idle(v);
    }
}

pub fn mainloop<P: Plugin>(plugin: &mut P) -> Result<(), ReadError> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut rpc_looper = RpcLoop::new(stdout);
    let mut dispatcher = Dispatcher::new(plugin);

    rpc_looper.mainloop(|| stdin.lock(), &mut dispatcher)
}

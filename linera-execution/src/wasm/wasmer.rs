// SPDX-License-Identifier: Apache-2.0

//! Code specific to the usage of the [Wasmer](https://wasmer.io/) runtime.

// Export the system interface used by a user application.
wit_bindgen_host_wasmer_rust::export!("../linera-sdk/system.wit");

// Import the interface implemented by a user application.
wit_bindgen_host_wasmer_rust::import!("../linera-sdk/application.wit");

use self::{application::Application, system::PollLoad};
use super::{
    async_boundary::{ContextForwarder, HostFuture},
    common::{self, Runtime, WasmRuntimeContext},
    WasmApplication, WasmExecutionError,
};
use crate::{ExecutionError, WritableStorage};
use std::{marker::PhantomData, mem, sync::Arc, task::Poll};
use tokio::sync::Mutex;
use wasmer::{imports, Module, RuntimeError, Store};

/// Type representing the [Wasmer](https://wasmer.io/) runtime.
///
/// The runtime has a lifetime so that it does not outlive the trait object used to export the
/// system API.
pub struct Wasmer<'storage> {
    _lifetime: PhantomData<&'storage ()>,
}

impl<'storage> Runtime for Wasmer<'storage> {
    type Application = Application;
    type Store = Store;
    type StorageGuard = StorageGuard<'storage>;
    type Error = RuntimeError;
}

impl WasmApplication {
    /// Prepare a runtime instance to call into the WASM application.
    pub fn prepare_runtime<'storage>(
        &self,
        storage: &'storage dyn WritableStorage,
    ) -> Result<WasmRuntimeContext<Wasmer<'storage>>, WasmExecutionError> {
        let mut store = Store::default();
        let module = Module::new(&store, &self.bytecode)
            .map_err(wit_bindgen_host_wasmer_rust::anyhow::Error::from)?;
        let mut imports = imports! {};
        let context_forwarder = ContextForwarder::default();
        let (system_api, storage_guard) = SystemApi::new(context_forwarder.clone(), storage);
        let system_api_setup = system::add_to_imports(&mut store, &mut imports, system_api);
        let (application, instance) =
            application::Application::instantiate(&mut store, &module, &mut imports)?;

        system_api_setup(&instance, &store)?;

        Ok(WasmRuntimeContext {
            context_forwarder,
            application,
            store,
            _storage_guard: storage_guard,
        })
    }
}

impl<'storage> common::Application<Wasmer<'storage>> for Application {
    fn execute_operation_new(
        &self,
        store: &mut Store,
        context: application::OperationContext,
        operation: &[u8],
    ) -> Result<application::ExecuteOperation, RuntimeError> {
        Application::execute_operation_new(self, store, context, operation)
    }

    fn execute_operation_poll(
        &self,
        store: &mut Store,
        future: &application::ExecuteOperation,
    ) -> Result<application::PollExecutionResult, RuntimeError> {
        Application::execute_operation_poll(self, store, future)
    }

    fn execute_effect_new(
        &self,
        store: &mut Store,
        context: application::EffectContext,
        effect: &[u8],
    ) -> Result<application::ExecuteEffect, RuntimeError> {
        Application::execute_effect_new(self, store, context, effect)
    }

    fn execute_effect_poll(
        &self,
        store: &mut Store,
        future: &application::ExecuteEffect,
    ) -> Result<application::PollExecutionResult, RuntimeError> {
        Application::execute_effect_poll(self, store, future)
    }

    fn call_application_new(
        &self,
        store: &mut Store,
        context: application::CalleeContext,
        argument: &[u8],
        forwarded_sessions: &[application::SessionId],
    ) -> Result<application::CallApplication, RuntimeError> {
        Application::call_application_new(self, store, context, argument, forwarded_sessions)
    }

    fn call_application_poll(
        &self,
        store: &mut Store,
        future: &application::CallApplication,
    ) -> Result<application::PollCallApplication, RuntimeError> {
        Application::call_application_poll(self, store, future)
    }

    fn call_session_new(
        &self,
        store: &mut Store,
        context: application::CalleeContext,
        session: application::SessionParam,
        argument: &[u8],
        forwarded_sessions: &[application::SessionId],
    ) -> Result<application::CallSession, RuntimeError> {
        Application::call_session_new(self, store, context, session, argument, forwarded_sessions)
    }

    fn call_session_poll(
        &self,
        store: &mut Store,
        future: &application::CallSession,
    ) -> Result<application::PollCallSession, RuntimeError> {
        Application::call_session_poll(self, store, future)
    }

    fn query_application_new(
        &self,
        store: &mut Store,
        context: application::QueryContext,
        argument: &[u8],
    ) -> Result<application::QueryApplication, RuntimeError> {
        Application::query_application_new(self, store, context, argument)
    }

    fn query_application_poll(
        &self,
        store: &mut Store,
        future: &application::QueryApplication,
    ) -> Result<application::PollQuery, RuntimeError> {
        Application::query_application_poll(self, store, future)
    }
}

/// Implementation to forward system calls from the guest WASM module to the host implementation.
pub struct SystemApi {
    context: ContextForwarder,
    storage: Arc<Mutex<Option<&'static dyn WritableStorage>>>,
}

impl SystemApi {
    /// Create a new [`SystemApi`] instance, ensuring that the lifetime of the [`WritableStorage`]
    /// trait object is respected.
    ///
    /// # Safety
    ///
    /// This method uses a [`mem::transmute`] call to erase the lifetime of the `storage` trait
    /// object reference. However, this is safe because the lifetime is transfered to the returned
    /// [`StorageGuard`], which removes the unsafe reference from memory when it is dropped,
    /// ensuring the lifetime is respected.
    ///
    /// The [`StorageGuard`] instance must be kept alive while the trait object is still expected to
    /// be alive and usable by the WASM application.
    pub fn new(context: ContextForwarder, storage: &dyn WritableStorage) -> (Self, StorageGuard) {
        let storage_without_lifetime = unsafe { mem::transmute(storage) };
        let storage = Arc::new(Mutex::new(Some(storage_without_lifetime)));

        let guard = StorageGuard {
            storage: storage.clone(),
            _lifetime: PhantomData,
        };

        (SystemApi { context, storage }, guard)
    }

    /// Safely obtain the [`WritableStorage`] trait object instance to handle a system call.
    ///
    /// # Panics
    ///
    /// If there is a concurrent call from the WASM application (which is impossible as long as it
    /// is executed in a single thread) or if the trait object is no longer alive (or more
    /// accurately, if the [`StorageGuard`] returned by [`Self::new`] was dropped to indicate it's
    /// no longer alive).
    fn storage(&self) -> &'static dyn WritableStorage {
        *self
            .storage
            .try_lock()
            .expect("Unexpected concurrent storage access by application")
            .as_ref()
            .expect("Application called storage after it should have stopped")
    }
}

impl system::System for SystemApi {
    type Load = HostFuture<'static, Result<Vec<u8>, ExecutionError>>;
    type LoadAndLock = HostFuture<'static, Result<Vec<u8>, ExecutionError>>;

    fn load_new(&mut self) -> Self::Load {
        HostFuture::new(self.storage().try_read_my_state())
    }

    fn load_poll(&mut self, future: &Self::Load) -> PollLoad {
        match future.poll(&mut self.context) {
            Poll::Pending => PollLoad::Pending,
            Poll::Ready(Ok(bytes)) => PollLoad::Ready(Ok(bytes)),
            Poll::Ready(Err(error)) => PollLoad::Ready(Err(error.to_string())),
        }
    }

    fn load_and_lock_new(&mut self) -> Self::LoadAndLock {
        HostFuture::new(self.storage().try_read_and_lock_my_state())
    }

    fn load_and_lock_poll(&mut self, future: &Self::LoadAndLock) -> PollLoad {
        match future.poll(&mut self.context) {
            Poll::Pending => PollLoad::Pending,
            Poll::Ready(Ok(bytes)) => PollLoad::Ready(Ok(bytes)),
            Poll::Ready(Err(error)) => PollLoad::Ready(Err(error.to_string())),
        }
    }

    fn store_and_unlock(&mut self, state: &[u8]) -> bool {
        self.storage()
            .save_and_unlock_my_state(state.to_owned())
            .is_ok()
    }
}

/// A guard to unsure that the [`WritableStorage`] trait object isn't called after it's no longer
/// borrowed.
pub struct StorageGuard<'storage> {
    storage: Arc<Mutex<Option<&'static dyn WritableStorage>>>,
    _lifetime: PhantomData<&'storage ()>,
}

impl Drop for StorageGuard<'_> {
    fn drop(&mut self) {
        self.storage
            .try_lock()
            .expect("Guard dropped while storage is still in use")
            .take();
    }
}

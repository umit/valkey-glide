// Copyright Valkey GLIDE Project Contributors - SPDX Identifier: Apache-2.0

#![deny(unsafe_op_in_unsafe_fn)]
use glide_core::client::Client as GlideClient;
use glide_core::cluster_scan_container::get_cluster_scan_cursor;
use glide_core::command_request::SimpleRoutes;
use glide_core::command_request::{Routes, SlotTypes};
use glide_core::connection_request;
use glide_core::errors;
use glide_core::errors::RequestErrorType;
use glide_core::request_type::RequestType;
use glide_core::ConnectionRequest;
use protobuf::Message;
use redis::cluster_routing::{
    MultipleNodeRoutingInfo, Route, RoutingInfo, SingleNodeRoutingInfo, SlotAddr,
};
use redis::cluster_routing::{ResponsePolicy, Routable};
use redis::ObjectType;
use redis::ScanStateRC;
use redis::{ClusterScanArgs, RedisError};
use redis::{Cmd, RedisResult, Value};
use std::ffi::CStr;
use std::slice::from_raw_parts;
use std::str;
use std::sync::Arc;
use std::{
    ffi::{c_void, CString},
    mem,
    os::raw::{c_char, c_double, c_long, c_ulong},
};
use tokio::runtime::Builder;
use tokio::runtime::Runtime;

/// The struct represents the response of the command.
///
/// It will have one of the value populated depending on the return type of the command.
///
/// The struct is freed by the external caller by using `free_command_response` to avoid memory leaks.
/// TODO: Add a type enum to validate what type of response is being sent in the CommandResponse.
#[repr(C)]
#[derive(Debug, Clone)]
pub struct CommandResponse {
    pub response_type: ResponseType,
    pub int_value: i64,
    pub float_value: c_double,
    pub bool_value: bool,

    /// Below two values are related to each other.
    /// `string_value` represents the string.
    /// `string_value_len` represents the length of the string.
    pub string_value: *mut c_char,
    pub string_value_len: c_long,

    /// Below two values are related to each other.
    /// `array_value` represents the array of CommandResponse.
    /// `array_value_len` represents the length of the array.
    pub array_value: *mut CommandResponse,
    pub array_value_len: c_long,

    /// Below two values represent the Map structure inside CommandResponse.
    /// The map is transformed into an array of (map_key: CommandResponse, map_value: CommandResponse) and passed to Go.
    /// These are represented as pointers as the map can be null (optionally present).
    pub map_key: *mut CommandResponse,
    pub map_value: *mut CommandResponse,

    /// Below two values are related to each other.
    /// `sets_value` represents the set of CommandResponse.
    /// `sets_value_len` represents the length of the set.
    pub sets_value: *mut CommandResponse,
    pub sets_value_len: c_long,
}

impl Default for CommandResponse {
    fn default() -> Self {
        CommandResponse {
            response_type: ResponseType::default(),
            int_value: 0,
            float_value: 0.0,
            bool_value: false,
            string_value: std::ptr::null_mut(),
            string_value_len: 0,
            array_value: std::ptr::null_mut(),
            array_value_len: 0,
            map_key: std::ptr::null_mut(),
            map_value: std::ptr::null_mut(),
            sets_value: std::ptr::null_mut(),
            sets_value_len: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Default, Clone)]
pub enum ResponseType {
    #[default]
    Null = 0,
    Int = 1,
    Float = 2,
    Bool = 3,
    String = 4,
    Array = 5,
    Map = 6,
    Sets = 7,
}

/// Success callback that is called when a command succeeds.
///
/// The success callback needs to copy the given string synchronously, since it will be dropped by Rust once the callback returns. The callback should be offloaded to a separate thread in order not to exhaust the client's thread pool.
///
/// `index_ptr` is a baton-pass back to the caller language to uniquely identify the promise.
/// `message` is the value returned by the command. The 'message' is managed by Rust and is freed when the callback returns control back to the caller.
pub type SuccessCallback =
    unsafe extern "C" fn(index_ptr: usize, message: *const CommandResponse) -> ();

/// Failure callback that is called when a command fails.
///
/// The failure callback needs to copy the given string synchronously, since it will be dropped by Rust once the callback returns. The callback should be offloaded to a separate thread in order not to exhaust the client's thread pool.
///
/// `index_ptr` is a baton-pass back to the caller language to uniquely identify the promise.
/// `error_message` is the error message returned by server for the failed command. The 'error_message' is managed by Rust and is freed when the callback returns control back to the caller.
/// `error_type` is the type of error returned by glide-core, depending on the `RedisError` returned.
pub type FailureCallback = unsafe extern "C" fn(
    index_ptr: usize,
    error_message: *const c_char,
    error_type: RequestErrorType,
) -> ();

/// The connection response.
///
/// It contains either a connection or an error. It is represented as a struct instead of a union for ease of use in the wrapper language.
///
/// The struct is freed by the external caller by using `free_connection_response` to avoid memory leaks.
#[repr(C)]
pub struct ConnectionResponse {
    pub conn_ptr: *const c_void,
    pub connection_error_message: *const c_char,
}

/// A `GlideClient` adapter.
pub struct ClientAdapter {
    runtime: Runtime,
    core: Arc<CommandExecutionCore>,
}

struct CommandExecutionCore {
    success_callback: SuccessCallback,
    failure_callback: FailureCallback,
    client: GlideClient,
}

fn create_client_internal(
    connection_request_bytes: &[u8],
    success_callback: SuccessCallback,
    failure_callback: FailureCallback,
) -> Result<ClientAdapter, String> {
    let request = connection_request::ConnectionRequest::parse_from_bytes(connection_request_bytes)
        .map_err(|err| err.to_string())?;
    // TODO: optimize this using multiple threads instead of a single worker thread (e.g. by pinning each go thread to a rust thread)
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .thread_name("Valkey-GLIDE Go thread")
        .build()
        .map_err(|err| {
            let redis_error = err.into();
            errors::error_message(&redis_error)
        })?;
    let client = runtime
        .block_on(GlideClient::new(ConnectionRequest::from(request), None))
        .map_err(|err| err.to_string())?;
    let core = Arc::new(CommandExecutionCore {
        success_callback,
        failure_callback,
        client,
    });
    Ok(ClientAdapter { runtime, core })
}

/// Creates a new `ClientAdapter` with a new `GlideClient` configured using a Protobuf `ConnectionRequest`.
///
/// The returned `ConnectionResponse` will only be freed by calling [`free_connection_response`].
///
/// `connection_request_bytes` is an array of bytes that will be parsed into a Protobuf `ConnectionRequest` object.
/// `connection_request_len` is the number of bytes in `connection_request_bytes`.
/// `success_callback` is the callback that will be called when a command succeeds.
/// `failure_callback` is the callback that will be called when a command fails.
///
/// # Safety
///
/// * `connection_request_bytes` must point to `connection_request_len` consecutive properly initialized bytes. It must be a well-formed Protobuf `ConnectionRequest` object. The array must be allocated by the caller and subsequently freed by the caller after this function returns.
/// * `connection_request_len` must not be greater than the length of the connection request bytes array. It must also not be greater than the max value of a signed pointer-sized integer.
/// * The `conn_ptr` pointer in the returned `ConnectionResponse` must live while the client is open/active and must be explicitly freed by calling [`close_client``].
/// * The `connection_error_message` pointer in the returned `ConnectionResponse` must live until the returned `ConnectionResponse` pointer is passed to [`free_connection_response``].
/// * Both the `success_callback` and `failure_callback` function pointers need to live while the client is open/active. The caller is responsible for freeing both callbacks.
// TODO: Consider making this async
#[no_mangle]
pub unsafe extern "C" fn create_client(
    connection_request_bytes: *const u8,
    connection_request_len: usize,
    success_callback: SuccessCallback,
    failure_callback: FailureCallback,
) -> *const ConnectionResponse {
    let request_bytes =
        unsafe { std::slice::from_raw_parts(connection_request_bytes, connection_request_len) };
    let response = match create_client_internal(request_bytes, success_callback, failure_callback) {
        Err(err) => ConnectionResponse {
            conn_ptr: std::ptr::null(),
            connection_error_message: CString::into_raw(
                CString::new(err).expect("Couldn't convert error message to CString"),
            ),
        },
        Ok(client) => ConnectionResponse {
            conn_ptr: Arc::into_raw(Arc::new(client)) as *const c_void,
            connection_error_message: std::ptr::null(),
        },
    };
    Box::into_raw(Box::new(response))
}

/// Closes the given `GlideClient`, freeing it from the heap.
///
/// `client_adapter_ptr` is a pointer to a valid `GlideClient` returned in the `ConnectionResponse` from [`create_client`].
///
/// # Panics
///
/// This function panics when called with a null `client_adapter_ptr`.
///
/// # Safety
///
/// * `close_client` can only be called once per client. Calling it twice is undefined behavior, since the address will be freed twice.
/// * `close_client` must be called after `free_connection_response` has been called to avoid creating a dangling pointer in the `ConnectionResponse`.
/// * `client_adapter_ptr` must be obtained from the `ConnectionResponse` returned from [`create_client`].
/// * `client_adapter_ptr` must be valid until `close_client` is called.
#[no_mangle]
pub unsafe extern "C" fn close_client(client_adapter_ptr: *const c_void) {
    assert!(!client_adapter_ptr.is_null());
    // This will bring the strong count down to 0 once all client requests are done.
    unsafe { Arc::decrement_strong_count(client_adapter_ptr as *const ClientAdapter) };
}

/// Deallocates a `ConnectionResponse`.
///
/// This function also frees the contained error. If the contained error is a null pointer, the function returns and only the `ConnectionResponse` is freed.
///
/// # Panics
///
/// This function panics when called with a null `ConnectionResponse` pointer.
///
/// # Safety
///
/// * `free_connection_response` can only be called once per `ConnectionResponse`. Calling it twice is undefined behavior, since the address will be freed twice.
/// * `connection_response_ptr` must be obtained from the `ConnectionResponse` returned from [`create_client`].
/// * `connection_response_ptr` must be valid until `free_connection_response` is called.
/// * The contained `connection_error_message` must be obtained from the `ConnectionResponse` returned from [`create_client`].
/// * The contained `connection_error_message` must be valid until `free_connection_response` is called and it must outlive the `ConnectionResponse` that contains it.
#[no_mangle]
pub unsafe extern "C" fn free_connection_response(
    connection_response_ptr: *mut ConnectionResponse,
) {
    assert!(!connection_response_ptr.is_null());
    let connection_response = unsafe { Box::from_raw(connection_response_ptr) };
    let connection_error_message = connection_response.connection_error_message;
    drop(connection_response);
    if !connection_error_message.is_null() {
        drop(unsafe { CString::from_raw(connection_error_message as *mut c_char) });
    }
}

/// Provides the string mapping for the ResponseType enum.
///
/// Important: the returned pointer is a pointer to a constant string and should not be freed.
#[no_mangle]
pub extern "C" fn get_response_type_string(response_type: ResponseType) -> *const c_char {
    let c_str = match response_type {
        ResponseType::Null => c"Null",
        ResponseType::Int => c"Int",
        ResponseType::Float => c"Float",
        ResponseType::Bool => c"Bool",
        ResponseType::String => c"String",
        ResponseType::Array => c"Array",
        ResponseType::Map => c"Map",
        ResponseType::Sets => c"Sets",
    };
    c_str.as_ptr()
}

/// Deallocates a `CommandResponse`.
///
/// This function also frees the contained string_value and array_value. If the string_value and array_value are null pointers, the function returns and only the `CommandResponse` is freed.
///
/// # Safety
///
/// * `free_command_response` can only be called once per `CommandResponse`. Calling it twice is undefined behavior, since the address will be freed twice.
/// * `command_response_ptr` must be obtained from the `CommandResponse` returned in [`SuccessCallback`] from [`command`].
/// * `command_response_ptr` must be valid until `free_command_response` is called.
#[no_mangle]
pub unsafe extern "C" fn free_command_response(command_response_ptr: *mut CommandResponse) {
    if !command_response_ptr.is_null() {
        let command_response = unsafe { Box::from_raw(command_response_ptr) };
        free_command_response_elements(*command_response);
    }
}

/// Frees the nested elements of `CommandResponse`.
/// TODO: Add a test case to check for memory leak.
///
/// # Safety
///
/// * `free_command_response_elements` can only be called once per `CommandResponse`. Calling it twice is undefined behavior, since the address will be freed twice.
/// * The contained `string_value` must be obtained from the `CommandResponse` returned in [`SuccessCallback`] from [`command`].
/// * The contained `string_value` must be valid until `free_command_response` is called and it must outlive the `CommandResponse` that contains it.
/// * The contained `array_value` must be obtained from the `CommandResponse` returned in [`SuccessCallback`] from [`command`].
/// * The contained `array_value` must be valid until `free_command_response` is called and it must outlive the `CommandResponse` that contains it.
/// * The contained `map_key` must be obtained from the `CommandResponse` returned in [`SuccessCallback`] from [`command`].
/// * The contained `map_key` must be valid until `free_command_response` is called and it must outlive the `CommandResponse` that contains it.
/// * The contained `map_value` must be obtained from the `CommandResponse` returned in [`SuccessCallback`] from [`command`].
/// * The contained `map_value` must be valid until `free_command_response` is called and it must outlive the `CommandResponse` that contains it.
fn free_command_response_elements(command_response: CommandResponse) {
    let string_value = command_response.string_value;
    let string_value_len = command_response.string_value_len;
    let array_value = command_response.array_value;
    let array_value_len = command_response.array_value_len;
    let map_key = command_response.map_key;
    let map_value = command_response.map_value;
    let sets_value = command_response.sets_value;
    let sets_value_len = command_response.sets_value_len;
    if !string_value.is_null() {
        let len = string_value_len as usize;
        unsafe { Vec::from_raw_parts(string_value, len, len) };
    }
    if !array_value.is_null() {
        let len = array_value_len as usize;
        let vec = unsafe { Vec::from_raw_parts(array_value, len, len) };
        for element in vec.into_iter() {
            free_command_response_elements(element);
        }
    }
    if !map_key.is_null() {
        unsafe { free_command_response(map_key) };
    }
    if !map_value.is_null() {
        unsafe { free_command_response(map_value) };
    }
    if !sets_value.is_null() {
        let len = sets_value_len as usize;
        let vec = unsafe { Vec::from_raw_parts(sets_value, len, len) };
        for element in vec.into_iter() {
            free_command_response_elements(element);
        }
    }
}

/// Frees the error_message received on a command failure.
/// TODO: Add a test case to check for memory leak.
///
/// # Panics
///
/// This functions panics when called with a null `c_char` pointer.
///
/// # Safety
///
/// `free_error_message` can only be called once per `error_message`. Calling it twice is undefined
/// behavior, since the address will be freed twice.
#[no_mangle]
pub unsafe extern "C" fn free_error_message(error_message: *mut c_char) {
    assert!(!error_message.is_null());
    drop(unsafe { CString::from_raw(error_message as *mut c_char) });
}

/// Converts a double pointer to a vec.
///
/// # Safety
///
/// `convert_double_pointer_to_vec` returns a `Vec` of u8 slice which holds pointers of `go`
/// strings. The returned `Vec<&'a [u8]>` is meant to be copied into Rust code. Storing them
/// for later use will cause the program to crash as the pointers will be freed by go's gc
unsafe fn convert_double_pointer_to_vec<'a>(
    data: *const *const c_void,
    len: c_ulong,
    data_len: *const c_ulong,
) -> Vec<&'a [u8]> {
    let string_ptrs = unsafe { from_raw_parts(data, len as usize) };
    let string_lengths = unsafe { from_raw_parts(data_len, len as usize) };
    let mut result = Vec::<&[u8]>::with_capacity(string_ptrs.len());
    for (i, &str_ptr) in string_ptrs.iter().enumerate() {
        let slice = unsafe { from_raw_parts(str_ptr as *const u8, string_lengths[i] as usize) };
        result.push(slice);
    }
    result
}

fn convert_vec_to_pointer<T>(mut vec: Vec<T>) -> (*mut T, c_long) {
    vec.shrink_to_fit();
    let vec_ptr = vec.as_mut_ptr();
    let len = vec.len() as c_long;
    mem::forget(vec);
    (vec_ptr, len)
}

/// TODO: Avoid the use of expect and unwrap in the code and add a common error handling mechanism.
fn valkey_value_to_command_response(value: Value) -> RedisResult<CommandResponse> {
    let mut command_response = CommandResponse::default();
    let result: RedisResult<CommandResponse> = match value {
        Value::Nil => Ok(command_response),
        Value::SimpleString(text) => {
            let vec: Vec<u8> = text.into_bytes();
            let (vec_ptr, len) = convert_vec_to_pointer(vec);
            command_response.string_value = vec_ptr as *mut c_char;
            command_response.string_value_len = len;
            command_response.response_type = ResponseType::String;
            Ok(command_response)
        }
        Value::BulkString(text) => {
            let (vec_ptr, len) = convert_vec_to_pointer(text);
            command_response.string_value = vec_ptr as *mut c_char;
            command_response.string_value_len = len;
            command_response.response_type = ResponseType::String;
            Ok(command_response)
        }
        Value::VerbatimString { format: _, text } => {
            let vec: Vec<u8> = text.into_bytes();
            let (vec_ptr, len) = convert_vec_to_pointer(vec);
            command_response.string_value = vec_ptr as *mut c_char;
            command_response.string_value_len = len;
            command_response.response_type = ResponseType::String;
            Ok(command_response)
        }
        Value::Okay => {
            let vec: Vec<u8> = String::from("OK").into_bytes();
            let (vec_ptr, len) = convert_vec_to_pointer(vec);
            command_response.string_value = vec_ptr as *mut c_char;
            command_response.string_value_len = len;
            command_response.response_type = ResponseType::String;
            Ok(command_response)
        }
        Value::Int(num) => {
            command_response.int_value = num;
            command_response.response_type = ResponseType::Int;
            Ok(command_response)
        }
        Value::Double(num) => {
            command_response.float_value = num;
            command_response.response_type = ResponseType::Float;
            Ok(command_response)
        }
        Value::Boolean(boolean) => {
            command_response.bool_value = boolean;
            command_response.response_type = ResponseType::Bool;
            Ok(command_response)
        }
        Value::Array(array) => {
            let vec: Vec<CommandResponse> = array
                .into_iter()
                .map(|v| {
                    valkey_value_to_command_response(v)
                        .expect("Value couldn't be converted to CommandResponse")
                })
                .collect();
            let (vec_ptr, len) = convert_vec_to_pointer(vec);
            command_response.array_value = vec_ptr;
            command_response.array_value_len = len;
            command_response.response_type = ResponseType::Array;
            Ok(command_response)
        }
        Value::Map(map) => {
            let result: Vec<CommandResponse> = map
                .into_iter()
                .map(|(key, val)| {
                    let mut map_response = CommandResponse::default();

                    let map_key = valkey_value_to_command_response(key)
                        .expect("Value couldn't be converted to CommandResponse");
                    map_response.map_key = Box::into_raw(Box::new(map_key));

                    let map_val = valkey_value_to_command_response(val)
                        .expect("Value couldn't be converted to CommandResponse");
                    map_response.map_value = Box::into_raw(Box::new(map_val));

                    map_response
                })
                .collect::<Vec<_>>();

            let (vec_ptr, len) = convert_vec_to_pointer(result);
            command_response.array_value = vec_ptr;
            command_response.array_value_len = len;
            command_response.response_type = ResponseType::Map;
            Ok(command_response)
        }
        Value::Set(array) => {
            let vec: Vec<CommandResponse> = array
                .into_iter()
                .map(|v| {
                    valkey_value_to_command_response(v)
                        .expect("Value couldn't be converted to CommandResponse")
                })
                .collect();
            let (vec_ptr, len) = convert_vec_to_pointer(vec);
            command_response.sets_value = vec_ptr;
            command_response.sets_value_len = len;
            command_response.response_type = ResponseType::Sets;
            Ok(command_response)
        }
        // TODO: Add support for other return types.
        _ => todo!(),
    };
    result
}

/// Executes a command.
///
/// # Safety
///
/// * `client_adapter_ptr` must not be `null` and must be obtained from the `ConnectionResponse` returned from [`create_client`].
/// * `client_adapter_ptr` must be able to be safely casted to a valid [`Arc<ClientAdapter>`] via [`Arc::from_raw`]. See the safety documentation of [`std::sync::Arc::from_raw`].
/// * `channel` must be Go channel pointer and must be valid until either `success_callback` or `failure_callback` is finished.
/// * `args` is an optional bytes pointers array. The array must be allocated by the caller and subsequently freed by the caller after this function returns.
/// * `args_len` is an optional bytes length array. The array must be allocated by the caller and subsequently freed by the caller after this function returns.
/// * `arg_count` the number of elements in `args` and `args_len`. It must also not be greater than the max value of a signed pointer-sized integer.
/// * `arg_count` must be 0 if `args` and `args_len` are null.
/// * `args` and `args_len` must either be both null or be both not null.
/// * `route_bytes` is an optional array of bytes that will be parsed into a Protobuf `Routes` object. The array must be allocated by the caller and subsequently freed by the caller after this function returns.
/// * `route_bytes_len` is the number of bytes in `route_bytes`. It must also not be greater than the max value of a signed pointer-sized integer.
/// * `route_bytes_len` must be 0 if `route_bytes` is null.
/// * This function should only be called should with a `client_adapter_ptr` created by [`create_client`], before [`close_client`] was called with the pointer.
#[no_mangle]
pub unsafe extern "C" fn command(
    client_adapter_ptr: *const c_void,
    channel: usize,
    command_type: RequestType,
    arg_count: c_ulong,
    args: *const usize,
    args_len: *const c_ulong,
    route_bytes: *const u8,
    route_bytes_len: usize,
) {
    let client_adapter = unsafe {
        // we increment the strong count to ensure that the client is not dropped just because we turned it into an Arc.
        Arc::increment_strong_count(client_adapter_ptr);
        Arc::from_raw(client_adapter_ptr as *mut ClientAdapter)
    };

    let core = client_adapter.core.clone();

    let arg_vec: Vec<&[u8]> = if !args.is_null() && !args_len.is_null() {
        unsafe { convert_double_pointer_to_vec(args as *const *const c_void, arg_count, args_len) }
    } else {
        Vec::new()
    };

    // Create the command outside of the task to ensure that the command arguments passed
    // from "go" are still valid
    let mut cmd = command_type
        .get_command()
        .expect("Couldn't fetch command type");
    for command_arg in arg_vec {
        cmd.arg(command_arg);
    }

    let route = if !route_bytes.is_null() {
        let r_bytes = unsafe { std::slice::from_raw_parts(route_bytes, route_bytes_len) };
        Routes::parse_from_bytes(r_bytes).unwrap()
    } else {
        Routes::default()
    };

    client_adapter.runtime.spawn(async move {
        let result = core
            .client
            .clone()
            .send_command(&cmd, get_route(route, Some(&cmd)))
            .await;
        let value = match result {
            Ok(value) => value,
            Err(err) => {
                let message = errors::error_message(&err);
                let error_type = errors::error_type(&err);

                let c_err_str = CString::into_raw(
                    CString::new(message).expect("Couldn't convert error message to CString"),
                );
                unsafe { (core.failure_callback)(channel, c_err_str, error_type) };
                return;
            }
        };

        let result: RedisResult<CommandResponse> = valkey_value_to_command_response(value);

        unsafe {
            match result {
                Ok(message) => (core.success_callback)(channel, Box::into_raw(Box::new(message))),
                Err(err) => {
                    let message = errors::error_message(&err);
                    let error_type = errors::error_type(&err);

                    let c_err_str = CString::into_raw(
                        CString::new(message).expect("Couldn't convert error message to CString"),
                    );
                    (core.failure_callback)(channel, c_err_str, error_type);
                }
            };
        }
    });
}

fn get_route(route: Routes, cmd: Option<&Cmd>) -> Option<RoutingInfo> {
    use glide_core::command_request::routes::Value;
    let route = route.value?;
    let get_response_policy = |cmd: Option<&Cmd>| {
        cmd.and_then(|cmd| {
            cmd.command()
                .and_then(|cmd| ResponsePolicy::for_command(&cmd))
        })
    };
    match route {
        Value::SimpleRoutes(simple_route) => {
            let simple_route = simple_route.enum_value().unwrap();
            match simple_route {
                SimpleRoutes::AllNodes => Some(RoutingInfo::MultiNode((
                    MultipleNodeRoutingInfo::AllNodes,
                    get_response_policy(cmd),
                ))),
                SimpleRoutes::AllPrimaries => Some(RoutingInfo::MultiNode((
                    MultipleNodeRoutingInfo::AllMasters,
                    get_response_policy(cmd),
                ))),
                SimpleRoutes::Random => {
                    Some(RoutingInfo::SingleNode(SingleNodeRoutingInfo::Random))
                }
            }
        }
        Value::SlotKeyRoute(slot_key_route) => Some(RoutingInfo::SingleNode(
            SingleNodeRoutingInfo::SpecificNode(Route::new(
                redis::cluster_topology::get_slot(slot_key_route.slot_key.as_bytes()),
                get_slot_addr(&slot_key_route.slot_type),
            )),
        )),
        Value::SlotIdRoute(slot_id_route) => Some(RoutingInfo::SingleNode(
            SingleNodeRoutingInfo::SpecificNode(Route::new(
                slot_id_route.slot_id as u16,
                get_slot_addr(&slot_id_route.slot_type),
            )),
        )),
        Value::ByAddressRoute(by_address_route) => match u16::try_from(by_address_route.port) {
            Ok(port) => Some(RoutingInfo::SingleNode(SingleNodeRoutingInfo::ByAddress {
                host: by_address_route.host.to_string(),
                port,
            })),
            Err(_) => {
                // TODO: Handle error propagation.
                None
            }
        },
        _ => panic!("unknown route type"),
    }
}

fn get_slot_addr(slot_type: &protobuf::EnumOrUnknown<SlotTypes>) -> SlotAddr {
    slot_type
        .enum_value()
        .map(|slot_type| match slot_type {
            SlotTypes::Primary => SlotAddr::Master,
            SlotTypes::Replica => SlotAddr::ReplicaRequired,
        })
        .expect("Received unexpected slot id type")
}

// This struct is used to keep track of the cursor of a cluster scan.
#[repr(C)]
pub struct ClusterScanCursor {
    cursor: *const c_char,
}

impl ClusterScanCursor {
    #[no_mangle]
    pub extern "C" fn new_cluster_cursor(new_cursor: *const c_char) -> Self {
        if !new_cursor.is_null() {
            ClusterScanCursor { cursor: new_cursor }
        } else {
            ClusterScanCursor::default()
        }
    }

    fn get_cursor(&self) -> *const c_char {
        self.cursor
    }
}

impl Default for ClusterScanCursor {
    fn default() -> Self {
        let default_value = CString::new("0").unwrap();
        ClusterScanCursor {
            cursor: default_value.into_raw(),
        }
    }
}

impl Drop for ClusterScanCursor {
    fn drop(&mut self) {
        let c_str = unsafe { CStr::from_ptr(self.cursor) };
        let temp_str = c_str.to_str().expect("Must be UTF-8");
        glide_core::cluster_scan_container::remove_scan_state_cursor(temp_str.to_string());
    }
}

/// CGO method which allows the Go client to request a cluster scan command to be executed.
///
/// `client_adapter_ptr` is a pointer to a valid `GlideClusterClient` returned in the `ConnectionResponse` from [`create_client`].
/// `channel` is a pointer to a valid payload buffer which is created in the Go client.
/// `cursor` is a ClusterScanCursor struct to hold relevant cursor information.
/// `arg_count` keeps track of how many option arguments are passed in the Go client.
/// `args` is a pointer to C string representation of the string args passed in Go.
/// `args_len` is a pointer to the lengths of the C string representation of the string args passed in Go.
/// `success_callback` is the callback that will be called when a command succeeds.
/// `failure_callback` is the callback that will be called when a command fails.
///
/// # Safety
///
/// * `client_adapter_ptr` must be obtained from the `ConnectionResponse` returned from [`create_client`].
/// * `client_adapter_ptr` must be valid until `close_client` is called.
/// * `channel` must be valid until it is passed in a call to [`free_command_response`].
/// * Both the `success_callback` and `failure_callback` function pointers need to live while the client is open/active. The caller is responsible for freeing both callbacks.
#[no_mangle]
pub unsafe extern "C" fn request_cluster_scan(
    client_adapter_ptr: *const c_void,
    channel: usize,
    cursor: ClusterScanCursor,
    arg_count: c_ulong,
    args: *const usize,
    args_len: *const c_ulong,
) {
    let client_adapter =
        unsafe { Box::leak(Box::from_raw(client_adapter_ptr as *mut ClientAdapter)) };
    let core = client_adapter.core.clone();

    let c_str = unsafe { CStr::from_ptr(cursor.get_cursor()) };
    let temp_str = c_str.to_str().expect("Must be UTF-8");
    let cursor_id = temp_str.to_string();

    let cluster_scan_args: ClusterScanArgs = if arg_count > 0 {
        let arg_vec = unsafe {
            convert_double_pointer_to_vec(args as *const *const c_void, arg_count, args_len)
        };

        let mut pattern: &[u8] = &[];
        let mut object_type: &[u8] = &[];
        let mut count: &[u8] = &[];

        for i in 0..arg_count as usize {
            match arg_vec[i] {
                b"MATCH" => pattern = arg_vec[i + 1],
                b"TYPE" => object_type = arg_vec[i + 1],
                b"COUNT" => count = arg_vec[i + 1],
                _ => {}
            }
        }

        // Convert back to proper types
        let converted_count = match str::from_utf8(count) {
            Ok(v) => {
                if !count.is_empty() {
                    str::parse::<u32>(v).unwrap()
                } else {
                    10 // default count value
                }
            }
            Err(e) => {
                let redis_err = RedisError::from(e);
                let message = errors::error_message(&redis_err);
                let error_type = errors::error_type(&redis_err);

                let c_err_str = CString::into_raw(
                    CString::new(message).expect("Couldn't convert error message to CString"),
                );
                unsafe { (core.failure_callback)(channel, c_err_str, error_type) };
                return;
            }
        };

        let converted_type = match str::from_utf8(object_type) {
            Ok(v) => ObjectType::from(v.to_string()),
            Err(e) => {
                let redis_err = RedisError::from(e);
                let message = errors::error_message(&redis_err);
                let error_type = errors::error_type(&redis_err);

                let c_err_str = CString::into_raw(
                    CString::new(message).expect("Couldn't convert error message to CString"),
                );
                unsafe { (core.failure_callback)(channel, c_err_str, error_type) };
                return;
            }
        };

        let mut cluster_scan_args_builder = ClusterScanArgs::builder();
        if !count.is_empty() {
            cluster_scan_args_builder = cluster_scan_args_builder.with_count(converted_count);
        }
        if !pattern.is_empty() {
            cluster_scan_args_builder = cluster_scan_args_builder.with_match_pattern(pattern);
        }
        if !object_type.is_empty() {
            cluster_scan_args_builder = cluster_scan_args_builder.with_object_type(converted_type);
        }
        cluster_scan_args_builder.build()
    } else {
        ClusterScanArgs::builder().build()
    };

    let scan_state_cursor = match get_cluster_scan_cursor(cursor_id) {
        Ok(existing_cursor) => existing_cursor,
        Err(_error) => ScanStateRC::new(),
    };
    let core = client_adapter.core.clone();

    client_adapter.runtime.spawn(async move {
        let result = core
            .client
            .clone()
            .cluster_scan(&scan_state_cursor, cluster_scan_args)
            .await;
        let value = match result {
            Ok(value) => value,
            Err(err) => {
                let message = errors::error_message(&err);
                let error_type = errors::error_type(&err);

                let c_err_str = CString::into_raw(
                    CString::new(message).expect("Couldn't convert error message to CString"),
                );
                unsafe { (core.failure_callback)(channel, c_err_str, error_type) };
                return;
            }
        };

        let result: RedisResult<CommandResponse> = valkey_value_to_command_response(value);

        unsafe {
            match result {
                Ok(message) => (core.success_callback)(channel, Box::into_raw(Box::new(message))),
                Err(err) => {
                    let message = errors::error_message(&err);
                    let error_type = errors::error_type(&err);

                    let c_err_str = CString::into_raw(
                        CString::new(message).expect("Couldn't convert error message to CString"),
                    );
                    (core.failure_callback)(channel, c_err_str, error_type);
                }
            };
        }
    });
}

/// CGO method which allows the Go client to request an update to the connection password.
///
/// `client_adapter_ptr` is a pointer to a valid `GlideClusterClient` returned in the `ConnectionResponse` from [`create_client`].
/// `channel` is a pointer to a valid payload buffer which is created in the Go client.
/// `password` is a pointer to C string representation of the password passed in Go.
/// `immediate_auth` is a boolean flag to indicate if the password should be updated immediately.
/// `success_callback` is the callback that will be called when a command succeeds.
/// `failure_callback` is the callback that will be called when a command fails.
///
/// # Safety
///
/// * `client_adapter_ptr` must be obtained from the `ConnectionResponse` returned from [`create_client`].
/// * `client_adapter_ptr` must be valid until `close_client` is called.
/// * `channel` must be valid until it is passed in a call to [`free_command_response`].
/// * Both the `success_callback` and `failure_callback` function pointers need to live while the client is open/active. The caller is responsible for freeing both callbacks.
#[no_mangle]
pub unsafe extern "C" fn update_connection_password(
    client_adapter_ptr: *const c_void,
    channel: usize,
    password: *const c_char,
    immediate_auth: bool,
) {
    let client_adapter =
        unsafe { Box::leak(Box::from_raw(client_adapter_ptr as *mut ClientAdapter)) };
    let core = client_adapter.core.clone();

    // argument conversion to be used in the async block
    let password = unsafe { CStr::from_ptr(password).to_str().unwrap() };
    let password_option = if password.is_empty() {
        None
    } else {
        Some(password.to_string())
    };

    client_adapter.runtime.spawn(async move {
        let result = core
            .client
            .clone()
            .update_connection_password(password_option, immediate_auth)
            .await;
        let value = match result {
            Ok(value) => value,
            Err(err) => {
                let message = errors::error_message(&err);
                let error_type = errors::error_type(&err);

                let c_err_str = CString::into_raw(
                    CString::new(message).expect("Couldn't convert error message to CString"),
                );
                unsafe { (core.failure_callback)(channel, c_err_str, error_type) };
                return;
            }
        };

        let result: RedisResult<CommandResponse> = valkey_value_to_command_response(value);

        unsafe {
            match result {
                Ok(message) => (core.success_callback)(channel, Box::into_raw(Box::new(message))),
                Err(err) => {
                    let message = errors::error_message(&err);
                    let error_type = errors::error_type(&err);

                    let c_err_str = CString::into_raw(
                        CString::new(message).expect("Couldn't convert error message to CString"),
                    );
                    (core.failure_callback)(channel, c_err_str, error_type);
                }
            };
        }
    });
}

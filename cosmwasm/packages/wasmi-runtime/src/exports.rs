use lazy_static::lazy_static;
use log::*;
use std::ffi::c_void;

use enclave_ffi_types::{
    Ctx, EnclaveBuffer, EnclaveError, HandleResult, HealthCheckResult, InitResult, QueryResult,
};
use std::panic;
use std::sync::SgxMutex;

use crate::results::{
    result_handle_success_to_handleresult, result_init_success_to_initresult,
    result_query_success_to_queryresult,
};
use crate::{
    oom_handler,
    utils::{validate_const_ptr, validate_mut_ptr},
};

lazy_static! {
    static ref ECALL_ALLOCATE_STACK: SgxMutex<Vec<EnclaveBuffer>> = SgxMutex::new(Vec::new());
}

/// Allocate a buffer in the enclave and return a pointer to it. This is useful for ocalls that
/// want to return a response of unknown length to the enclave. Instead of pre-allocating it on the
/// ecall side, the ocall can call this ecall and return the EnclaveBuffer to the ecall that called
/// it.
///
/// host -> ecall_x -> ocall_x -> ecall_allocate
/// # Safety
/// Always use protection
#[no_mangle]
pub unsafe extern "C" fn ecall_allocate(buffer: *const u8, length: usize) -> EnclaveBuffer {
    if let Err(_err) = oom_handler::register_oom_handler() {
        error!("Could not register OOM handler!");
        return EnclaveBuffer::default();
    }

    if let Err(_e) = validate_const_ptr(buffer, length as usize) {
        error!("Tried to access data outside enclave memory space!");
        return EnclaveBuffer::default();
    }

    let slice = std::slice::from_raw_parts(buffer, length);
    let result = panic::catch_unwind(|| {
        let vector_copy = slice.to_vec();
        let boxed_vector = Box::new(vector_copy);
        let heap_pointer = Box::into_raw(boxed_vector);
        let enclave_buffer = EnclaveBuffer {
            ptr: heap_pointer as *mut c_void,
        };
        ECALL_ALLOCATE_STACK
            .lock()
            .unwrap()
            .push(enclave_buffer.unsafe_clone());
        enclave_buffer
    });

    if let Err(_err) = oom_handler::restore_safety_buffer() {
        error!("Could not restore OOM safety buffer!");
        return EnclaveBuffer::default();
    }

    result.unwrap_or_else(|err| {
        // We can get here only by failing to allocate memory,
        // so there's no real need here to test if oom happened
        error!("Enclave ran out of memory: {:?}", err);
        oom_handler::get_then_clear_oom_happened();
        EnclaveBuffer::default()
    })
}

/// Take a pointer as returned by `ecall_allocate` and recover the Vec<u8> inside of it.
/// # Safety
///  This is a text
pub unsafe fn recover_buffer(ptr: EnclaveBuffer) -> Option<Vec<u8>> {
    if ptr.ptr.is_null() {
        return None;
    }

    let mut alloc_stack = ECALL_ALLOCATE_STACK.lock().unwrap();

    // search the stack from the end for this pointer
    let maybe_index = alloc_stack
        .iter()
        .rev()
        .position(|buffer| buffer.ptr as usize == ptr.ptr as usize);
    if let Some(index_from_the_end) = maybe_index {
        // This index is probably at the end of the stack, but we give it a little more flexibility
        // in case access patterns change in the future
        let index = alloc_stack.len() - index_from_the_end - 1;
        alloc_stack.swap_remove(index);
    } else {
        return None;
    }
    let boxed_vector = Box::from_raw(ptr.ptr as *mut Vec<u8>);
    Some(*boxed_vector)
}

/// # Safety
/// Always use protection
#[no_mangle]
pub unsafe extern "C" fn ecall_init(
    context: Ctx,
    gas_limit: u64,
    used_gas: *mut u64,
    contract: *const u8,
    contract_len: usize,
    env: *const u8,
    env_len: usize,
    msg: *const u8,
    msg_len: usize,
    sig_info: *const u8,
    sig_info_len: usize,
) -> InitResult {
    if let Err(err) = oom_handler::register_oom_handler() {
        error!("Could not register OOM handler!");
        return InitResult::Failure { err };
    }
    if let Err(_e) = validate_mut_ptr(used_gas as _, std::mem::size_of::<u64>()) {
        error!("Tried to access data outside enclave memory!");
        return result_init_success_to_initresult(Err(EnclaveError::FailedFunctionCall));
    }
    if let Err(_e) = validate_const_ptr(env, env_len as usize) {
        error!("Tried to access data outside enclave memory!");
        return result_init_success_to_initresult(Err(EnclaveError::FailedFunctionCall));
    }
    if let Err(_e) = validate_const_ptr(msg, msg_len as usize) {
        error!("Tried to access data outside enclave memory!");
        return result_init_success_to_initresult(Err(EnclaveError::FailedFunctionCall));
    }
    if let Err(_e) = validate_const_ptr(contract, contract_len as usize) {
        error!("Tried to access data outside enclave memory!");
        return result_init_success_to_initresult(Err(EnclaveError::FailedFunctionCall));
    }
    if let Err(_e) = validate_const_ptr(sig_info, sig_info_len as usize) {
        error!("Tried to access data outside enclave memory!");
        return result_init_success_to_initresult(Err(EnclaveError::FailedFunctionCall));
    }

    let contract = std::slice::from_raw_parts(contract, contract_len);
    let env = std::slice::from_raw_parts(env, env_len);
    let msg = std::slice::from_raw_parts(msg, msg_len);
    let sig_info = std::slice::from_raw_parts(sig_info, sig_info_len);
    let result = panic::catch_unwind(|| {
        let mut local_used_gas = *used_gas;
        let result = crate::wasm::init(
            context,
            gas_limit,
            &mut local_used_gas,
            contract,
            env,
            msg,
            sig_info,
        );
        *used_gas = local_used_gas;
        result_init_success_to_initresult(result)
    });

    if let Err(err) = oom_handler::restore_safety_buffer() {
        error!("Could not restore OOM safety buffer!");
        return InitResult::Failure { err };
    }

    if let Ok(res) = result {
        res
    } else {
        *used_gas = gas_limit / 2;

        if oom_handler::get_then_clear_oom_happened() {
            error!("Call ecall_init failed because the enclave ran out of memory!");
            InitResult::Failure {
                err: EnclaveError::OutOfMemory,
            }
        } else {
            error!("Call ecall_init panic'd unexpectedly!");
            InitResult::Failure {
                err: EnclaveError::Panic,
            }
        }
    }
}

/// # Safety
/// Always use protection
#[no_mangle]
pub unsafe extern "C" fn ecall_handle(
    context: Ctx,
    gas_limit: u64,
    used_gas: *mut u64,
    contract: *const u8,
    contract_len: usize,
    env: *const u8,
    env_len: usize,
    msg: *const u8,
    msg_len: usize,
    sig_info: *const u8,
    sig_info_len: usize,
) -> HandleResult {
    if let Err(err) = oom_handler::register_oom_handler() {
        error!("Could not register OOM handler!");
        return HandleResult::Failure { err };
    }
    if let Err(_e) = validate_mut_ptr(used_gas as _, std::mem::size_of::<u64>()) {
        error!("Tried to access data outside enclave memory!");
        return result_handle_success_to_handleresult(Err(EnclaveError::FailedFunctionCall));
    }
    if let Err(_e) = validate_const_ptr(env, env_len as usize) {
        error!("Tried to access data outside enclave memory!");
        return result_handle_success_to_handleresult(Err(EnclaveError::FailedFunctionCall));
    }
    if let Err(_e) = validate_const_ptr(msg, msg_len as usize) {
        error!("Tried to access data outside enclave memory!");
        return result_handle_success_to_handleresult(Err(EnclaveError::FailedFunctionCall));
    }
    if let Err(_e) = validate_const_ptr(contract, contract_len as usize) {
        error!("Tried to access data outside enclave memory!");
        return result_handle_success_to_handleresult(Err(EnclaveError::FailedFunctionCall));
    }
    if let Err(_e) = validate_const_ptr(sig_info, sig_info_len as usize) {
        error!("Tried to access data outside enclave memory!");
        return result_handle_success_to_handleresult(Err(EnclaveError::FailedFunctionCall));
    }

    let contract = std::slice::from_raw_parts(contract, contract_len);
    let env = std::slice::from_raw_parts(env, env_len);
    let msg = std::slice::from_raw_parts(msg, msg_len);
    let sig_info = std::slice::from_raw_parts(sig_info, sig_info_len);
    let result = panic::catch_unwind(|| {
        let mut local_used_gas = *used_gas;
        let result = crate::wasm::handle(
            context,
            gas_limit,
            &mut local_used_gas,
            contract,
            env,
            msg,
            sig_info,
        );
        *used_gas = local_used_gas;
        result_handle_success_to_handleresult(result)
    });

    if let Err(err) = oom_handler::restore_safety_buffer() {
        error!("Could not restore OOM safety buffer!");
        return HandleResult::Failure { err };
    }

    if let Ok(res) = result {
        res
    } else {
        *used_gas = gas_limit / 2;

        if oom_handler::get_then_clear_oom_happened() {
            error!("Call ecall_handle failed because the enclave ran out of memory!");
            HandleResult::Failure {
                err: EnclaveError::OutOfMemory,
            }
        } else {
            error!("Call ecall_handle panic'd unexpectedly!");
            HandleResult::Failure {
                err: EnclaveError::Panic,
            }
        }
    }
}

/// # Safety
/// Always use protection
#[no_mangle]
pub unsafe extern "C" fn ecall_query(
    context: Ctx,
    gas_limit: u64,
    used_gas: *mut u64,
    contract: *const u8,
    contract_len: usize,
    msg: *const u8,
    msg_len: usize,
) -> QueryResult {
    if let Err(err) = oom_handler::register_oom_handler() {
        error!("Could not register OOM handler!");
        return QueryResult::Failure { err };
    }
    if let Err(_e) = validate_mut_ptr(used_gas as _, std::mem::size_of::<u64>()) {
        error!("Tried to access data outside enclave memory!");
        return result_query_success_to_queryresult(Err(EnclaveError::FailedFunctionCall));
    }
    if let Err(_e) = validate_const_ptr(msg, msg_len as usize) {
        error!("Tried to access data outside enclave memory!");
        return result_query_success_to_queryresult(Err(EnclaveError::FailedFunctionCall));
    }
    if let Err(_e) = validate_const_ptr(contract, contract_len as usize) {
        error!("Tried to access data outside enclave memory!");
        return result_query_success_to_queryresult(Err(EnclaveError::FailedFunctionCall));
    }

    let contract = std::slice::from_raw_parts(contract, contract_len);
    let msg = std::slice::from_raw_parts(msg, msg_len);
    let result = panic::catch_unwind(|| {
        let mut local_used_gas = *used_gas;
        let result = crate::wasm::query(context, gas_limit, &mut local_used_gas, contract, msg);
        *used_gas = local_used_gas;
        result_query_success_to_queryresult(result)
    });

    if let Err(err) = oom_handler::restore_safety_buffer() {
        error!("Could not restore OOM safety buffer!");
        return QueryResult::Failure { err };
    }

    if let Ok(res) = result {
        res
    } else {
        *used_gas = gas_limit / 2;

        if oom_handler::get_then_clear_oom_happened() {
            error!("Call ecall_query failed because the enclave ran out of memory!");
            QueryResult::Failure {
                err: EnclaveError::OutOfMemory,
            }
        } else {
            error!("Call ecall_query panic'd unexpectedly!");
            QueryResult::Failure {
                err: EnclaveError::Panic,
            }
        }
    }
}

/// # Safety
/// Always use protection
#[no_mangle]
pub unsafe extern "C" fn ecall_health_check() -> HealthCheckResult {
    HealthCheckResult::Success
}

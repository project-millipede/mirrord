#[macro_export]
macro_rules! replace {
    ($interceptor:expr, $detour_name:expr, $detour_function:expr, $detour_type:ty, $hook_fn:expr) => {{
        let intercept = |interceptor: &mut frida_gum::interceptor::Interceptor,
                         symbol_name,
                         detour: $detour_type|
         -> Result<$detour_type, LayerError> {
            tracing::info!("replace -> hooking {:#?}", $detour_name);

            let function = frida_gum::Module::find_export_by_name(None, symbol_name)
                .ok_or(LayerError::NoExportName(symbol_name.to_string()))?;

            let replaced = interceptor.replace(
                function,
                frida_gum::NativePointer(detour as *mut libc::c_void),
                frida_gum::NativePointer(std::ptr::null_mut()),
            );

            tracing::info!(
                "replace -> hooked {:#?} {:#?}",
                $detour_name,
                replaced.is_ok()
            );

            let original_fn: $detour_type = std::mem::transmute(replaced?);

            Ok(original_fn)
        };

        intercept($interceptor, $detour_name, $detour_function)
            .and_then(|hooked| Ok($hook_fn.set(hooked).unwrap()))
    }};
}

#[cfg(target_os = "linux")]
macro_rules! hook_symbol {
    ($interceptor:expr, $func:expr, $detour_name:expr, $binary:expr) => {
        if let Some(symbol) = frida_gum::Module::find_symbol_by_name($binary, $func) {
            match $interceptor.replace(
                symbol,
                frida_gum::NativePointer($detour_name as *mut libc::c_void),
                frida_gum::NativePointer(std::ptr::null_mut::<libc::c_void>()),
            ) {
                Err(e) => {
                    tracing::debug!("{} error: {:?}", $func, e);
                }
                Ok(_) => {
                    tracing::debug!("{} hooked", $func);
                }
            }
        };
    };
}

#[cfg(target_os = "linux")]
pub(crate) use hook_symbol;

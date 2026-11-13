//! Release the owned P2 request-options handle consumed by outgoing-handler.
//! Wasmtime 48.0.1 clones these options but leaves their table entry allocated.
//! Keep its HTTP implementation and policy hooks; repair only that ownership gap.
use super::StoreState;
use wasmtime::component::{Linker, Resource};
use wasmtime_wasi_http::p2::bindings::http::{outgoing_handler, types};
use wasmtime_wasi_http::WasiHttpView;

pub(super) fn add_to_linker(linker: &mut Linker<StoreState>) -> wasmtime::Result<()> {
    linker.allow_shadowing(true);
    let result = (|| {
        linker
            .instance("wasi:http/outgoing-handler@0.2.12")?
            .func_wrap(
                "handle",
                |mut store,
                 (request, options): (
                    Resource<types::OutgoingRequest>,
                    Option<Resource<types::RequestOptions>>,
                )| {
                    let state = store.data_mut();
                    let options_rep = options.as_ref().map(Resource::rep);
                    let result =
                        outgoing_handler::Host::handle(&mut state.http(), request, options);
                    if let Some(rep) = options_rep {
                        let handle = Resource::<types::RequestOptions>::new_own(rep);
                        // Do not double-delete if a future upstream implementation has
                        // already consumed the owned options correctly.
                        if state.table.get(&handle).is_ok() {
                            state.table.delete(handle)?;
                        }
                    }
                    let result = match result {
                        Ok(response) => Ok(response),
                        Err(error) => Err(error.downcast()?),
                    };
                    Ok((result,))
                },
            )
    })();
    linker.allow_shadowing(false);
    result
}

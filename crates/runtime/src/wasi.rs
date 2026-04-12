use wasmtime_wasi::{WasiCtxBuilder, WasiCtx, WasiView, ResourceTable};
pub struct MyState {
    pub ctx: WasiCtx,
    pub table: ResourceTable,
}
impl WasiView for MyState {
    fn table(&mut self) -> &mut ResourceTable { &mut self.table }
    fn ctx(&mut self) -> &mut WasiCtx { &mut self.ctx }
}

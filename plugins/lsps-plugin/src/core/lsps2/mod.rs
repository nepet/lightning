pub mod htlc;
pub mod htlc_holder;
pub mod manager;
pub mod persistence;
pub mod provider;
pub mod psbt;
pub mod service;
pub mod session;
pub mod timeouts;

// Note: handler.rs exists in this directory but is NOT exported.
// It contains legacy/duplicate code from before the MPP refactoring:
// - Duplicate HtlcAcceptedHookHandler (now in htlc.rs)
// - Duplicate provider traits (now in provider.rs)
// - Duplicate ClnApiRpc (now in cln_adapters/rpc.rs)
// - Duplicate Lsps2ServiceHandler (now in service.rs)
//
// The file has outdated imports and won't compile with the current structure.
// TODO: Delete handler.rs after verifying all test scenarios are covered
// by the tests in htlc.rs, provider.rs, and service.rs.

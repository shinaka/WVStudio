//! The PawnIO device, through raw IOCTLs on `\\?\GLOBALROOT\Device\PawnIO` (never PawnIOLib.dll): opening
//! it, loading one module per handle (LOAD), calling the module's functions (EXECUTE) and asking the
//! driver's version. In tests, opening always fails, so no test reaches the device.

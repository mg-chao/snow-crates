// Cursor adapter — dead code after multiplexer migration.
// Retained as an empty module for cfg(not(feature = "cursor")) compilation.
// The standalone cursor path now uses CursorStreamHandle registered
// directly with the StreamMultiplexer (see multiplexer_setup.rs).

/*
File: android/mod.rs

Purpose:
Android JNI plumbing shared by the platform backends.

Submodules:
- jni_ctx: attaching to the JVM through the android-activity `ndk-context` and loading app
  classes with the application class loader (Android only).
- saf: the pending-request registry of the SAF bridge and its class name. The registry is pure
  Rust and is compiled for host tests too.
*/

#[cfg(target_os = "android")]
pub(crate) mod jni_ctx;
pub(crate) mod saf;

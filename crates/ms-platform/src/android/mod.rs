/*
File: android/mod.rs

Purpose:
Android JNI plumbing shared by the platform backends.

Submodules:
- jni_ctx: attaching to the JVM through the android-activity `ndk-context` and loading app
  classes with the application class loader (Android only).
- saf: the pure half of the SAF bridge (class name, wire codes and decoders, pending-request
  registry, staging name rules). Pure Rust, compiled for host tests too.
*/

#[cfg(target_os = "android")]
pub(crate) mod jni_ctx;
pub(crate) mod saf;

//! An ElevenLabs Conversational AI-compatible service, backed by LiveKit.
//!
//! Happy's server calls `api.elevenlabs.io/v1/convai` to start voice sessions and to
//! meter them. This crate serves the same two endpoints against a self-hosted LiveKit
//! deployment, so pointing Happy here is a base-URL change and the unmodified
//! `@elevenlabs/*` SDKs keep working.
//!
//! # The shape of a request
//!
//! [`app`] joins the two halves of the HTTP surface — [`api`], which Happy calls, and
//! [`web`], the browser client — over the shared [`state`]. Everything below them is
//! arranged so that the request path has nothing left to check. [`config`] turns the environment into values known
//! to exist, [`conversation`] makes a badly-named room unrepresentable, [`record`] is
//! the single value describing a started conversation, [`livekit`] is the only place
//! that performs I/O against the SFU or signs a token, and [`store`] is where a
//! conversation outlives the room it happened in.

pub mod api;
pub mod app;
pub mod config;
pub mod conversation;
pub mod livekit;
pub mod record;
pub mod state;
pub mod store;
pub mod usage;
pub mod web;
pub mod webhook;

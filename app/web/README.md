# Development

The web crate defines the entrypoint for the web app along with any assets, components and dependencies that are specific to web builds. The web crate starts out something like this:

```
web/
├─ assets/ # Assets used by the web app - Any platform specific assets should go in this folder
├─ src/
│  ├─ main.rs # The entrypoint for the web app.It also defines the routes for the web platform
│  ├─ views/ # The views each route will render in the web version of the app
│  │  ├─ mod.rs # Defines the module for the views route and re-exports the components for each route
│  │  ├─ blog.rs # The component that will render at the /blog/:id route
│  │  ├─ home.rs # The component that will render at the / route
├─ Cargo.toml # The web crate's Cargo.toml - This should include all web specific dependencies
```

## Dependencies
This crate will only be included in the web build, so you should add all web specific dependencies to this crate's [Cargo.toml](../Cargo.toml) file instead of the shared [ui](../ui/Cargo.toml) crate.

### Serving Your Web App

You can start your web app with the following command:

```bash
dx serve
```

### Release Builds & the `wasm-release` Profile

Production wasm builds run through `dx build --release --platform web`, which uses the
`[profile.wasm-release]` defined in the [workspace Cargo.toml](../../Cargo.toml).

That profile sets `panic = "abort"` to minimize wasm binary size. As a consequence,
**panic unwinding does not work inside the wasm app**: `catch_unwind` and dioxus
`ErrorBoundary` will never trigger. Validate input at component boundaries instead of
relying on in-wasm panic recovery. If you ever need `ErrorBoundary`, remove
`panic = "abort"` from `[profile.wasm-release]` first (and expect a larger binary).

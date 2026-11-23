# arr: bread event integration

breadarr is a standalone media library manager: it works exactly the same
with or without `breadd` running. The app id `arr` is registered in the
`bread` repository's `KNOWN_APPS`, but this build does not open a
`bread-client` connection, publish events, or subscribe to commands. See the
parent `bread` repo's `Documentation.md`, specifically its "Namespaces" and
"Integrating a bread\* app" sections, for the convention a future
integration would follow.

App id: **`arr`**. Transport: none in this build. breadarr uses
`bread-utils` only for its XDG path helper, not the `bread-client` feature.

## Events published (`bread.arr.*`)

None.

## Commands honored (`bread.command.arr.*`)

None.

## Not implemented

- No `bread.arr.*` event is emitted. Library, download, import, and health
  activity stay inside breadarr and are not mirrored onto the bus.
- No `bread.command.arr.*` verb is honored. There is no remote-triggered
  library action in this build.
- Registering the app id is not an integration: it reserves the namespace
  for a future change. Do not add event or command rows here until the code
  that implements them exists.

## Fail-safe behavior

breadarr makes no bread socket connections in this build, so there is no bus
path to degrade. Its media management behavior is identical whether breadd
is installed, absent, or restarted.

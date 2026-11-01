# The `entroximer` engine binary, with the tools it runs (ffmpeg, yt-dlp, Deno) on its PATH.
{
  lib,
  rustPlatform,
  makeBinaryWrapper,
  pkg-config,
  cmake,
  libopus,
  ffmpeg-headless,
  yt-dlp,
  deno,
  python3,
}:

let
  engine = ../engine;
  # tools the engine starts at runtime:
  # - ffmpeg seeks and remuxes media
  # - yt-dlp resolves and downloads tracks, and needs a JavaScript runtime for YouTube
  # - Python lets the engine manage its own yt-dlp (`--ytdlp-managed`)
  runtimeTools = [
    ffmpeg-headless
    yt-dlp
    deno
    python3
  ];
in
rustPlatform.buildRustPackage {
  pname = "entroximer";
  version = (lib.importTOML (engine + "/Cargo.toml")).workspace.package.version;

  src = lib.fileset.toSource {
    root = engine;
    fileset = lib.fileset.unions [
      (engine + "/Cargo.toml")
      (engine + "/Cargo.lock")
      (engine + "/.cargo")
      (engine + "/crates")
    ];
  };
  cargoLock.lockFile = engine + "/Cargo.lock";

  cargoBuildFlags = [
    "--package"
    "entroximer-server"
  ];
  cargoTestFlags = [ "--workspace" ];

  nativeBuildInputs = [
    pkg-config
    cmake
    makeBinaryWrapper
  ];
  buildInputs = [ libopus ];
  # the media tests encode their test tone with ffmpeg's libopus
  nativeCheckInputs = [ ffmpeg-headless ];

  # MALLOC_ARENA_MAX: glibc otherwise keeps freed memory in an arena per thread, and the engine's
  # memory grows with every track played
  postInstall = ''
    wrapProgram $out/bin/entroximer \
      --prefix PATH : ${lib.makeBinPath runtimeTools} \
      --set-default MALLOC_ARENA_MAX 2
  '';

  meta = {
    description = "Discord audio engine with seekable streaming, effect graphs and YouTube support";
    # `license` is left out until the project has one
    mainProgram = "entroximer";
    platforms = lib.platforms.linux;
  };
}

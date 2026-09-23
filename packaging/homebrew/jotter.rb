# Template for Formula/jotter.rb in nfishel48/homebrew-tap. release.yml's
# publish-tap job fills in @VERSION@ and @SHA256@ and pushes the result; edit
# this file, not the tap's copy.
#
# A formula, not a cask, on purpose. Jotter ships as Jotter.app because macOS
# grants system-audio access only to a bundle, and that bundle is ad-hoc signed
# rather than notarized. Homebrew quarantines cask downloads, so Gatekeeper
# would refuse the app on first launch and Homebrew no longer offers a way
# around that; a formula's download is not quarantined. Once releases are
# signed with a Developer ID and notarized, a cask installing to /Applications
# is the better shape.
class Jotter < Formula
  desc "Record meetings as two tracks and transcribe them on-device"
  homepage "https://github.com/nfishel48/Jotter"
  url "https://github.com/nfishel48/Jotter/releases/download/v@VERSION@/Jotter-@VERSION@-macos-arm64.zip"
  sha256 "@SHA256@"
  license "MIT"

  depends_on arch: :arm64
  depends_on :macos

  def install
    # The zip holds nothing but Jotter.app, so Homebrew has already stepped
    # inside it: the working directory is the bundle, and `Contents` is what
    # there is to install.
    (prefix/"Jotter.app").install "Contents"
    # `jotter` resolves this symlink to find the bundle around itself, and
    # starts the recorder through that bundle so macOS attributes the capture
    # to Jotter rather than to the terminal.
    bin.install_symlink prefix/"Jotter.app/Contents/MacOS/jotter"
  end

  def caveats
    <<~EOS
      Fetch the speech models once (~675 MB):
        jotter models pull

      The first recording asks for Microphone and Screen & System Audio
      Recording access for Jotter. Jotter is not notarized yet, so macOS treats
      every upgrade as a new app and asks again.
    EOS
  end

  test do
    assert_equal "jotter #{version}", shell_output("#{bin}/jotter --version").strip
    assert_path_exists prefix/"Jotter.app/Contents/Info.plist"
  end
end

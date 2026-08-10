class FloppyMac < Formula
  desc "Read-only macOS FUSE driver for mounting floppy disks via Greaseweazle"
  homepage "https://github.com/smartlime/mac-floppy-driver"
  url "https://github.com/smartlime/mac-floppy-driver/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "FILL_IN_AFTER_GITHUB_RELEASE"
  license "GPL-2.0-or-later"

  depends_on "rust" => :build

  # macFUSE is a cask; install it separately:
  #   brew install --cask macfuse
  # The formula does not declare it as a dependency because casks cannot be
  # formula dependencies, but the binary will refuse to mount without it.

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    # --list-devices exits 0 and lists available serial ports (may be empty)
    assert_match(/Последовательные порты/, shell_output("#{bin}/floppy_mac --list-devices"))
  end
end

rust-mode: A major emacs mode for editing Rust source code
==========================================================

`rust-mode` makes editing [Rust](http://rust-lang.org) code with emacs
enjoyable.


### Manual Installation

To install manually, check out this repository and add this to your .emacs
file:

    (add-to-list 'load-path "/path/to/rust-mode/")
    (require 'rust-mode)

Rust mode will automatically be associated with .rs files. To enable it
explicitly, do `M-x rust-mode`.

### package.el installation via Marmalade or MELPA

It can be more convenient to use Emacs's package manager to handle
installation for you if you use many elisp libraries. If you have
package.el but haven't added Marmalade or MELPA, the community package source,
yet, add this to ~/.emacs.d/init.el:

Using Marmalade:

```lisp
(require 'package)
(add-to-list 'package-archives
             '("marmalade" . "http://marmalade-repo.org/packages/"))
(package-initialize)
```

Using MELPA:

```lisp
(require 'package)
(add-to-list 'package-archives
             '("melpa" . "http://melpa.milkbox.net/packages/") t)
(package-initialize)
```

Then do this to load the package listing:

* <kbd>M-x eval-buffer</kbd>
* <kbd>M-x package-refresh-contents</kbd>

If you use a version of Emacs prior to 24 that doesn't include
package.el, you can get it from http://bit.ly/pkg-el23.

If you have an older ELPA package.el installed from tromey.com, you
should upgrade in order to support installation from multiple sources.
The ELPA archive is deprecated and no longer accepting new packages,
so the version there (1.7.1) is very outdated.

#### Install rust-mode

From there you can install rust-mode or any other modes by choosing
them from a list:

* <kbd>M-x package-list-packages</kbd>

Now, to install packages, move your cursor to them and press i. This
will mark the packages for installation. When you're done with
marking, press x, and ELPA will install the packages for you (under
~/.emacs.d/elpa/).

* or using <kbd>M-x package-install rust-mode

### Tests via ERT

The file `rust-mode-tests.el` contains tests that can be run via ERT.  You can
use `run_rust_emacs_tests.sh` to run them in batch mode, if emacs is somewhere
in your `$PATH`.

### Known bugs

* Combining `global-whitespace-mode` and `rust-mode` is generally glitchy.
  See [Issue #3994](https://github.com/mozilla/rust/issues/3994).

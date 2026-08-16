# Bundled highlight queries

These are the `nvim-treesitter` highlight queries shipped by the Neovim
grammar package used by the matching dotfiles. Wren compiles them into the
provider binary so highlighting behaves the same without depending on a
runtime Neovim installation or a network connection.

Inherited query families (`c`, `ecma`, `jsx`, `html_tags`, and `php_only`) are
combined in `wren-provider` before Tree-sitter compiles them.

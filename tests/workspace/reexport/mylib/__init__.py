# mylib/__init__.py - Main package that re-exports from submodules
from mylib.modules import Linear  # Re-export from subpackage
from mylib.modules.linear import OriginalClass as AliasedClass  # Aliased re-export
from mylib.star_module import *  # Star import

"""Independent GTK clipboard peer. Only used in isolated Xvfb tests."""
import ctypes as c
import ctypes.util
import pathlib
import sys

gtk = c.CDLL(ctypes.util.find_library("gtk-3"))
gdk = c.CDLL(ctypes.util.find_library("gdk-3"))


def declare(lib, name, result, *args):
    fn = getattr(lib, name)
    fn.restype = result
    fn.argtypes = args
    return fn


declare(gtk, "gtk_init", None, c.c_void_p, c.c_void_p)(None, None)
atom = declare(gdk, "gdk_atom_intern_static_string", c.c_void_p, c.c_char_p)
clipboard = declare(gtk, "gtk_clipboard_get", c.c_void_p, c.c_void_p)(atom(b"CLIPBOARD"))
target = sys.argv[2].encode()
expected = pathlib.Path(sys.argv[3]).read_bytes()
if sys.argv[1] == "read":
    selection = declare(gtk, "gtk_clipboard_wait_for_contents", c.c_void_p,
                        c.c_void_p, c.c_void_p)(clipboard, atom(target))
    assert selection, "GTK could not retrieve the target"
    length = declare(gtk, "gtk_selection_data_get_length", c.c_int, c.c_void_p)(selection)
    ptr = declare(gtk, "gtk_selection_data_get_data", c.c_void_p, c.c_void_p)(selection)
    assert length == len(expected), (length, len(expected))
    assert c.string_at(ptr, length) == expected, "GTK received different bytes"
    declare(gtk, "gtk_selection_data_free", None, c.c_void_p)(selection)
else:
    class Target(c.Structure):
        _fields_ = [("target", c.c_char_p), ("flags", c.c_uint), ("info", c.c_uint)]

    get_callback = c.CFUNCTYPE(None, c.c_void_p, c.c_void_p, c.c_uint, c.c_void_p)
    clear_callback = c.CFUNCTYPE(None, c.c_void_p, c.c_void_p)
    set_data = declare(gtk, "gtk_selection_data_set", None, c.c_void_p,
                       c.c_void_p, c.c_int, c.c_char_p, c.c_int)

    @get_callback
    def get_data(_clipboard, selection, _info, _user):
        set_data(selection, atom(target), 8, expected, len(expected))

    @clear_callback
    def clear(_clipboard, _user):
        pass

    targets = (Target * 1)(Target(target, 0, 0))
    assert declare(gtk, "gtk_clipboard_set_with_data", c.c_int, c.c_void_p,
                   c.POINTER(Target), c.c_uint, get_callback, clear_callback, c.c_void_p)(
                       clipboard, targets, 1, get_data, clear, None)
    print("READY", flush=True)
    declare(gtk, "gtk_main", None)()

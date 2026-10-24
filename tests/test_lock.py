import os

import main as main_module


def test_lock_acquire_release(tmp_path, monkeypatch):
    monkeypatch.setattr(main_module, "DATA_DIR", str(tmp_path))
    monkeypatch.setattr(main_module, "LOCK_FILE", str(tmp_path / "bot.lock"))
    assert main_module._acquire_lock()
    assert os.path.exists(str(tmp_path / "bot.lock"))
    main_module._release_lock()
    assert not os.path.exists(str(tmp_path / "bot.lock"))

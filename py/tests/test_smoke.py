def test_imports() -> None:
    import susq
    import susq_bridge

    assert susq.__version__
    assert susq_bridge.__version__

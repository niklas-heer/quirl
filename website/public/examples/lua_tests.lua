return {
  test_arithmetic = function()
    assert(20 + 22 == 42)
  end,

  test_sdk_is_available = function()
    assert(type(quirl.cwd()) == "string")
  end,
}

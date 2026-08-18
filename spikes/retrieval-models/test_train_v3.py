"""Contract tests for product-aligned candidate calibration."""

import unittest

from train_v3 import (
    PRODUCT_TEST_COUNT,
    PRODUCT_TEST_FORCED,
    PRODUCT_VALIDATION_COUNT,
    PRODUCT_VALIDATION_FORCED,
    stratified_split,
)


class ProductCalibrationContractTests(unittest.TestCase):
    def test_stratified_split_has_product_validation_and_test_coverage(self) -> None:
        product = {f"product-{index}" for index in range(30)}
        product |= set(PRODUCT_TEST_FORCED) | set(PRODUCT_VALIDATION_FORCED)
        auxiliary = {f"auxiliary-{index}" for index in range(50)}
        eligible = product | auxiliary
        training, validation, test = stratified_split(eligible, product)
        self.assertEqual(len(validation & product), PRODUCT_VALIDATION_COUNT)
        self.assertEqual(len(test & product), PRODUCT_TEST_COUNT)
        self.assertTrue(PRODUCT_VALIDATION_FORCED <= validation)
        self.assertTrue(PRODUCT_TEST_FORCED <= test)
        self.assertFalse(training & validation)
        self.assertFalse(training & test)
        self.assertFalse(validation & test)
        self.assertEqual(training | validation | test, eligible)

    def test_stratified_split_is_deterministic(self) -> None:
        product = {f"product-{index}" for index in range(30)}
        product |= set(PRODUCT_TEST_FORCED) | set(PRODUCT_VALIDATION_FORCED)
        eligible = product | {f"auxiliary-{index}" for index in range(50)}
        self.assertEqual(
            stratified_split(eligible, product), stratified_split(eligible, product)
        )


if __name__ == "__main__":
    unittest.main()

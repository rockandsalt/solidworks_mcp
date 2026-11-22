#include "pch.h"
#include "service/SketchHelpers.h"
#include <catch2/catch_test_macros.hpp>
#include <algorithm>

namespace
{
	bool Has(const std::vector<Eigen::Vector2d>& points, const Eigen::Vector2d& p)
	{
		return std::any_of(points.begin(), points.end(), [&](const Eigen::Vector2d& q) { return (q - p).norm() < 1e-12; });
	}
}

// An arc's bounding points are its ends and the quadrant points it passes,
// going its own way round.
TEST_CASE("an arc's bounding points are its ends and the quadrant points it passes")
{
	using sld_mcp::service::ArcBoundingPoints;
	const Eigen::Vector2d c(1.0, 2.0);
	const Eigen::Vector2d east = c + Eigen::Vector2d(2.0, 0.0), north = c + Eigen::Vector2d(0.0, 2.0);
	const Eigen::Vector2d west = c + Eigen::Vector2d(-2.0, 0.0), south = c + Eigen::Vector2d(0.0, -2.0);
	const double s = std::sqrt(2.0);
	const Eigen::Vector2d northEast = c + Eigen::Vector2d(s, s), northWest = c + Eigen::Vector2d(-s, s);

	SECTION("a quarter counterclockwise from east to north passes no other quadrant")
	{
		const auto quarter = ArcBoundingPoints(c, 2.0, east, north, false);
		CHECK(Has(quarter, east));
		CHECK(Has(quarter, north));
		CHECK_FALSE(Has(quarter, west));
		CHECK_FALSE(Has(quarter, south));
	}
	SECTION("the same ends clockwise go the long way round, through south and west")
	{
		const auto longWay = ArcBoundingPoints(c, 2.0, east, north, true);
		CHECK(Has(longWay, west));
		CHECK(Has(longWay, south));
	}
	SECTION("from north-east to north-west counterclockwise: only north between")
	{
		const auto top = ArcBoundingPoints(c, 2.0, northEast, northWest, false);
		CHECK(top.size() == 3);
		CHECK(Has(top, north));
		CHECK_FALSE(Has(top, east));
		CHECK_FALSE(Has(top, west));
	}
}

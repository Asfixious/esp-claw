#include "color_detect.hpp"

#include <algorithm>
#include <cstdint>

static const char *TAG = "ColorDetect";

ColorDetectBase::ColorDetectBase(uint16_t width, uint16_t height) :
    m_width(width),
    m_height(height),
    m_hsv(nullptr),
    m_hsv_mask(heap_caps_malloc(width * height, MALLOC_CAP_DEFAULT | MALLOC_CAP_SIMD)),
    m_hsv_mask_cvmat(width, height, CV_8UC1, m_hsv_mask)
{
}

ColorDetectBase::~ColorDetectBase()
{
    heap_caps_free(m_hsv);
    heap_caps_free(m_hsv_mask);
}

void ColorDetectBase::register_color(const std::array<uint8_t, 3> &hsv_min,
                                     const std::array<uint8_t, 3> &hsv_max,
                                     const std::string &name,
                                     int area_thr)
{
    if (!(hsv_min[0] <= 180 && hsv_max[0] <= 180 && hsv_min[1] < hsv_max[1] && hsv_min[2] < hsv_max[2])) {
        ESP_LOGE(TAG, "Invalid hsv threshold.");
        return;
    }
    if (area_thr >= m_width * m_height) {
        ESP_LOGE(TAG, "Invalid area_thr.");
        return;
    }
    if (std::find(m_color_names.begin(), m_color_names.end(), name) != m_color_names.end()) {
        ESP_LOGE(TAG, "Color %s is already registered.", name.c_str());
        return;
    }
    if (m_hsv_min.size() == 1) {
        m_hsv = heap_caps_malloc(m_width * m_height * 3, MALLOC_CAP_DEFAULT);
        if (!m_hsv) {
            ESP_LOGE(TAG, "Failed to malloc memory.");
            return;
        }
    }
    m_hsv_min.emplace_back(hsv_min);
    m_hsv_max.emplace_back(hsv_max);
    m_area_thr.emplace_back(area_thr);
    m_color_names.emplace_back(name);
}

void ColorDetectBase::delete_color(int idx)
{
    assert(idx >= 0 && idx < m_hsv_min.size());
    m_hsv_min.erase(m_hsv_min.begin() + idx);
    m_hsv_max.erase(m_hsv_max.begin() + idx);
    m_area_thr.erase(m_area_thr.begin() + idx);
    m_color_names.erase(m_color_names.begin() + idx);
    if (m_hsv_min.size() == 1) {
        heap_caps_free(m_hsv);
        m_hsv = nullptr;
    }
}

void ColorDetectBase::delete_color(const std::string &name)
{
    auto it = std::find(m_color_names.begin(), m_color_names.end(), name);
    if (it == m_color_names.end()) {
        ESP_LOGE(TAG, "Failed to delete Color %s, it is not registered.", name.c_str());
        return;
    }
    delete_color(std::distance(m_color_names.begin(), it));
}

std::string ColorDetectBase::get_color_name(int idx)
{
    if (idx < 0 || idx >= m_hsv_min.size()) {
        ESP_LOGE(TAG, "Invalid index.");
        return {};
    }
    return m_color_names[idx];
}

int ColorDetectBase::get_color_num()
{
    return m_hsv_min.size();
}

ColorDetect::ColorDetect(uint16_t width, uint16_t height) :
    ColorDetectBase(width, height),
    m_morphology(false),
    m_hsv_mask_label(heap_caps_malloc(width * height * 2, MALLOC_CAP_DEFAULT | MALLOC_CAP_SIMD)),
    m_hsv_mask_label_cvmat(width, height, CV_16U, m_hsv_mask_label)
{
}

ColorDetect::~ColorDetect()
{
    heap_caps_free(m_hsv_mask_label);
}

void ColorDetect::enable_morphology(int kernel_size)
{
    m_morphology = true;
    m_kernel = cv::getStructuringElement(cv::MORPH_RECT, cv::Size(kernel_size, kernel_size));
}

std::list<dl::detect::result_t> &ColorDetect::run(const dl::image::img_t &img)
{
    m_result.clear();
    int n = get_color_num();
    if (n == 0) {
        ESP_LOGE(TAG, "No color is registered. Please call register_color() first.");
        return m_result;
    }
    dl::image::img_t hsv_mask_img = {
        .data = m_hsv_mask, .width = m_width, .height = m_height, .pix_type = dl::image::DL_IMAGE_PIX_TYPE_HSV_MASK};
    if (n == 1) {
        m_T.reset().set_src_img(img).set_dst_img(hsv_mask_img).set_hsv_thr(m_hsv_min[0], m_hsv_max[0]).transform();
        hsv_mask_process(0, m_T.get_scale_x(true), m_T.get_scale_y(true), img.width, img.height);
    } else {
        dl::image::img_t hsv_img = {
            .data = m_hsv, .width = m_width, .height = m_height, .pix_type = dl::image::DL_IMAGE_PIX_TYPE_HSV};
        m_T.reset().set_src_img(img).set_dst_img(hsv_img).transform();
        float scale_x = m_T.get_scale_x(true);
        float scale_y = m_T.get_scale_y(true);
        m_T.reset().set_src_img(hsv_img).set_dst_img(hsv_mask_img);
        for (int i = 0; i < n; i++) {
            m_T.set_hsv_thr(m_hsv_min[i], m_hsv_max[i]).transform();
            hsv_mask_process(i, scale_x, scale_y, img.width, img.height);
        }
    }
    return m_result;
}

bool ColorDetect::run_best(const dl::image::img_t &img, int max_blob_pixels, box_result_t *out)
{
    if (out == nullptr) {
        return false;
    }

    bool has_best = false;
    box_result_t best = {};
    int n = get_color_num();
    if (n == 0) {
        ESP_LOGE(TAG, "No color is registered. Please call register_color() first.");
        return false;
    }

    dl::image::img_t hsv_mask_img = {
        .data = m_hsv_mask, .width = m_width, .height = m_height, .pix_type = dl::image::DL_IMAGE_PIX_TYPE_HSV_MASK};
    if (n == 1) {
        m_T.reset().set_src_img(img).set_dst_img(hsv_mask_img).set_hsv_thr(m_hsv_min[0], m_hsv_max[0]).transform();
        hsv_mask_process_best(
            0, m_T.get_scale_x(true), m_T.get_scale_y(true), img.width, img.height, max_blob_pixels, &best, &has_best);
    } else {
        dl::image::img_t hsv_img = {
            .data = m_hsv, .width = m_width, .height = m_height, .pix_type = dl::image::DL_IMAGE_PIX_TYPE_HSV};
        m_T.reset().set_src_img(img).set_dst_img(hsv_img).transform();
        float scale_x = m_T.get_scale_x(true);
        float scale_y = m_T.get_scale_y(true);
        m_T.reset().set_src_img(hsv_img).set_dst_img(hsv_mask_img);
        for (int i = 0; i < n; i++) {
            m_T.set_hsv_thr(m_hsv_min[i], m_hsv_max[i]).transform();
            hsv_mask_process_best(
                i, scale_x, scale_y, img.width, img.height, max_blob_pixels, &best, &has_best);
        }
    }

    if (has_best) {
        *out = best;
    }
    return has_best;
}

void ColorDetect::hsv_mask_process(
    int color_id, float inv_scale_x, float inv_scale_y, uint16_t limit_width, uint16_t limit_height)
{
    if (m_morphology) {
        cv::morphologyEx(m_hsv_mask_cvmat, m_hsv_mask_cvmat, cv::MORPH_OPEN, m_kernel);
        cv::morphologyEx(m_hsv_mask_cvmat, m_hsv_mask_cvmat, cv::MORPH_CLOSE, m_kernel);
    }
    scan_hsv_mask(color_id, inv_scale_x, inv_scale_y, limit_width, limit_height, 0, nullptr, nullptr, true);
}

void ColorDetect::hsv_mask_process_best(int color_id,
                                        float inv_scale_x,
                                        float inv_scale_y,
                                        uint16_t limit_width,
                                        uint16_t limit_height,
                                        int max_blob_pixels,
                                        box_result_t *best,
                                        bool *has_best)
{
    if (m_morphology) {
        cv::morphologyEx(m_hsv_mask_cvmat, m_hsv_mask_cvmat, cv::MORPH_OPEN, m_kernel);
        cv::morphologyEx(m_hsv_mask_cvmat, m_hsv_mask_cvmat, cv::MORPH_CLOSE, m_kernel);
    }
    scan_hsv_mask(color_id,
                  inv_scale_x,
                  inv_scale_y,
                  limit_width,
                  limit_height,
                  max_blob_pixels,
                  best,
                  has_best,
                  false);
}

void ColorDetect::update_best_result(const box_result_t &candidate,
                                     int max_blob_pixels,
                                     box_result_t *best,
                                     bool *has_best)
{
    if (best == nullptr || has_best == nullptr) {
        return;
    }
    if (max_blob_pixels > 0 && candidate.area > max_blob_pixels) {
        return;
    }
    if (!*has_best || candidate.area > best->area) {
        *best = candidate;
        *has_best = true;
    }
}

void ColorDetect::scan_hsv_mask(int color_id,
                                float inv_scale_x,
                                float inv_scale_y,
                                uint16_t limit_width,
                                uint16_t limit_height,
                                int max_blob_pixels,
                                box_result_t *best,
                                bool *has_best,
                                bool collect_results)
{
    uint8_t *mask = static_cast<uint8_t *>(m_hsv_mask);
    uint16_t *stack = static_cast<uint16_t *>(m_hsv_mask_label);
    const int width = m_width;
    const int height = m_height;
    const int pixels = width * height;

    if (mask == nullptr || stack == nullptr || pixels <= 0 || pixels > UINT16_MAX) {
        ESP_LOGE(TAG, "Invalid color detect mask buffer.");
        return;
    }

    for (int start = 0; start < pixels; start++) {
        if (mask[start] == 0) {
            continue;
        }

        int top = 0;
        int area = 0;
        int min_x = width;
        int min_y = height;
        int max_x = 0;
        int max_y = 0;

        mask[start] = 0;
        stack[top++] = static_cast<uint16_t>(start);
        while (top > 0) {
            const int offset = stack[--top];
            const int x = offset % width;
            const int y = offset / width;
            area++;
            min_x = std::min(min_x, x);
            min_y = std::min(min_y, y);
            max_x = std::max(max_x, x);
            max_y = std::max(max_y, y);

            for (int dy = -1; dy <= 1; dy++) {
                const int ny = y + dy;
                if (ny < 0 || ny >= height) {
                    continue;
                }
                for (int dx = -1; dx <= 1; dx++) {
                    if (dx == 0 && dy == 0) {
                        continue;
                    }
                    const int nx = x + dx;
                    if (nx < 0 || nx >= width) {
                        continue;
                    }
                    const int next = ny * width + nx;
                    if (mask[next] == 0) {
                        continue;
                    }
                    mask[next] = 0;
                    stack[top++] = static_cast<uint16_t>(next);
                }
            }
        }

        if (area < m_area_thr[color_id]) {
            continue;
        }

        box_result_t candidate = {
            .category = color_id,
            .score = 1.f,
            .left = static_cast<int>(min_x * inv_scale_x),
            .top = static_cast<int>(min_y * inv_scale_y),
            .right = static_cast<int>((max_x + 1) * inv_scale_x),
            .bottom = static_cast<int>((max_y + 1) * inv_scale_y),
            .area = 0,
        };
        candidate.left = DL_CLIP(candidate.left, 0, limit_width - 1);
        candidate.top = DL_CLIP(candidate.top, 0, limit_height - 1);
        candidate.right = DL_CLIP(candidate.right, 0, limit_width - 1);
        candidate.bottom = DL_CLIP(candidate.bottom, 0, limit_height - 1);
        candidate.area = std::max(0, candidate.right - candidate.left + 1) *
                         std::max(0, candidate.bottom - candidate.top + 1);
        if (candidate.area <= 0) {
            continue;
        }

        if (collect_results) {
            dl::detect::result_t res = {
                candidate.category,
                candidate.score,
                {candidate.left, candidate.top, candidate.right, candidate.bottom},
                {},
            };
            m_result.push_back(res);
        } else {
            update_best_result(candidate, max_blob_pixels, best, has_best);
        }
    }
}

ColorRotateDetect::ColorRotateDetect(uint16_t width, uint16_t height, int kernel_size) :
    ColorDetectBase(width, height),
    m_kernel(cv::getStructuringElement(cv::MORPH_RECT, cv::Size(kernel_size, kernel_size)))
{
}

std::vector<ColorRotateDetect::result_t> &ColorRotateDetect::run(const dl::image::img_t &img)
{
    m_result.clear();
    int n = get_color_num();
    if (n == 0) {
        ESP_LOGE(TAG, "No color is registered. Please call register_color() first.");
        return m_result;
    }
    dl::image::img_t hsv_mask_img = {
        .data = m_hsv_mask, .width = m_width, .height = m_height, .pix_type = dl::image::DL_IMAGE_PIX_TYPE_HSV_MASK};
    if (n == 1) {
        m_T.reset().set_src_img(img).set_dst_img(hsv_mask_img).set_hsv_thr(m_hsv_min[0], m_hsv_max[0]).transform();
        hsv_mask_process(0, m_T.get_scale_x(true), m_T.get_scale_y(true));
    } else {
        dl::image::img_t hsv_img = {
            .data = m_hsv, .width = m_width, .height = m_height, .pix_type = dl::image::DL_IMAGE_PIX_TYPE_HSV};
        m_T.reset().set_src_img(img).set_dst_img(hsv_img).transform();
        float scale_x = m_T.get_scale_x(true);
        float scale_y = m_T.get_scale_y(true);
        m_T.reset().set_src_img(hsv_img).set_dst_img(hsv_mask_img);
        for (int i = 0; i < n; i++) {
            m_T.set_hsv_thr(m_hsv_min[i], m_hsv_max[i]).transform();
            hsv_mask_process(i, scale_x, scale_y);
        }
    }
    return m_result;
}

void ColorRotateDetect::hsv_mask_process(int color_id, float inv_scale_x, float inv_scale_y)
{
    cv::morphologyEx(m_hsv_mask_cvmat, m_hsv_mask_cvmat, cv::MORPH_OPEN, m_kernel);
    cv::morphologyEx(m_hsv_mask_cvmat, m_hsv_mask_cvmat, cv::MORPH_CLOSE, m_kernel);
    std::vector<std::vector<cv::Point>> contours;
    cv::findContours(m_hsv_mask_cvmat, contours, cv::RETR_EXTERNAL, cv::CHAIN_APPROX_SIMPLE);
    for (const auto &contour : contours) {
        double area = cv::contourArea(contour);
        if (area < m_area_thr[color_id]) {
            continue;
        }
        cv::RotatedRect rot_rect = cv::minAreaRect(contour);
        double angle_rad = rot_rect.angle * CV_PI / 180;
        float sin = (float)std::sin(angle_rad);
        float cos = (float)std::cos(angle_rad);
        float width = std::sqrt(std::pow(rot_rect.size.width * cos * inv_scale_x, 2) +
                                std::pow(rot_rect.size.width * sin * inv_scale_y, 2));
        float height = std::sqrt(std::pow(rot_rect.size.height * sin * inv_scale_x, 2) +
                                 std::pow(rot_rect.size.height * cos * inv_scale_y, 2));
        m_result.emplace_back(color_id,
                              cv::RotatedRect{{rot_rect.center.x * inv_scale_x, rot_rect.center.y * inv_scale_y},
                                              {width, height},
                                              rot_rect.angle});
    }
}
